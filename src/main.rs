#![no_std]
#![no_main]

esp_bootloader_esp_idf::esp_app_desc!(); // ESP-IDF App Descriptor required by newer espflash

#[allow(unused_imports)]
use esp_backtrace as _;

use log::info;
use core::fmt::Write;
use heapless::String;
use esp_hal::{
    delay::Delay,
    gpio::{DriveMode, Level, Output, OutputConfig},
    main,
    rng::Rng,
    spi::{self, master::Spi},
    time::Rate
};
use mipidsi::{
    interface::SpiInterface,
    models::ST7735s
};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_graphics::{
    Drawable,
    framebuffer::{Framebuffer, buffer_size},
    image::GetPixel,
    mono_font::{MonoTextStyle, ascii::FONT_8X13},
    pixelcolor::{Rgb565, raw::{LittleEndian, RawU16}},
    prelude::*,
    primitives::Rectangle,
    text::Text
};

const FRAMEDELAY_MS: u32 = 100;   // extra time added to the end of each frame
const CELL_SIZE: usize = 2;     // pixel size of each cell in the grid

const SCREEN_WIDTH: usize = 128;
const SCREEN_HEIGHT: usize = 160;
const GRID_WIDTH: usize = SCREEN_WIDTH/CELL_SIZE;
const GRID_HEIGHT: usize = SCREEN_HEIGHT/CELL_SIZE;

type ConwayGrid = [[bool; GRID_WIDTH]; GRID_HEIGHT];
type ConwayFramebuffer = Framebuffer<Rgb565, RawU16, LittleEndian, SCREEN_WIDTH, SCREEN_HEIGHT, {buffer_size::<Rgb565>(SCREEN_WIDTH, SCREEN_HEIGHT)}>;

fn write_text<D: DrawTarget<Color = Rgb565>>(display: &mut D, str: &str, row: i32) -> Result<(), D::Error> {
    Text::new(
        str,
        Point::new(8, row*13),
        MonoTextStyle::new(&FONT_8X13, Rgb565::CSS_ORANGE)
    )
    .draw(display)?;
    Ok(())
}

fn write_num<D: DrawTarget<Color = Rgb565>>(display: &mut D, generation: usize, row: i32) -> Result<(), D::Error> {
    let mut num_str = String::<20>::new();
    write!(num_str, "{generation}").unwrap();
    write_text(display, &num_str, row)
}

fn randomize_grid(rng: &mut Rng, grid: &mut ConwayGrid) {
    for row in grid.iter_mut() {
        for cell in row.iter_mut() {
            // Read a single byte from the RNG
            let mut buf = [0u8; 1];
            rng.read(&mut buf);

            // Set the cell to be alive or dead based on the random byte
            *cell = buf[0] < 64;
        }
    }
}

// Apply the Game of Life rules:
// 1. Any live cell with fewer than two live neighbors dies, as if by underpopulation.
// 2. Any live cell with two or three live neighbors lives on to the next generation.
// 3. Any live cell with more than three live neighbors dies, as if by overpopulation.
// 4. Any dead cell with exactly three live neighbors becomes a live cell, as if by reproduction.
fn update_game_of_life(grid: ConwayGrid) -> ConwayGrid {
    let mut new_grid = [[false; GRID_WIDTH]; GRID_HEIGHT];
    for y in 0..GRID_HEIGHT {
        for x in 0..GRID_WIDTH {
            new_grid[y][x] = match count_alive_neighbors(x, y, &grid) {
                2 if (grid[y][x]) => true,  // 2 neighbors iff cell is already alive (rule 2)
                3 => true,                  // 3 neighbors always (rule 2 + rule 4)
                _ => false                  // rule 1 + rule 3
            };
        }
    }
    new_grid
}

fn count_alive_neighbors(x: usize, y: usize, grid: &ConwayGrid) -> u8 {
    let mut count = 0;
    for i in 0..3 {
        for j in 0..3 {
            if i == 1 && j == 1 {
                continue; // skip the current cell itself
            }
            // calculate neighbor coordinates with wrapping
            let neighbor_x = (x + i + GRID_WIDTH - 1) % GRID_WIDTH;
            let neighbor_y = (y + j + GRID_HEIGHT - 1) % GRID_HEIGHT;

            if grid[neighbor_y][neighbor_x] {
                count += 1;
            }
        }
    }
    count
}

fn draw_grid(display: &mut ConwayFramebuffer, grid: &ConwayGrid, surviving_cell_color: Rgb565) {
    let old = display.clone();
    let cell_size = Size::new(CELL_SIZE as u32, CELL_SIZE as u32);
    for (y, row) in grid.iter().enumerate() {
        for (x, &cell) in row.iter().enumerate() {
            let cell_rect = Rectangle::new(Point::new((x * CELL_SIZE) as i32, (y * CELL_SIZE) as i32), cell_size);

            let cell_color = match cell {
                // live cell
                true => match count_alive_neighbors(x, y, grid) {
                    2 | 3 => surviving_cell_color, // will survive
                    _ => Rgb565::WHITE // will die this generation
                },
                // dead cell
                false => {
                    // fade out previous color (makes a cute little trail effect
                    let pixel = old.pixel(cell_rect.center()).unwrap_or(Rgb565::BLACK);
                    Rgb565::new(pixel.r()/2, pixel.g()/3, pixel.b()/3)
                }
            };
            let _ = display.fill_solid(&cell_rect, cell_color);
        }
    }
}

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    info!("Hello, ESP!");

    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut backlight = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    backlight.set_low();

    let mut delay = Delay::new();
    let mut rng = Rng::new();

    let lcd_spi = Spi::new(
        peripherals.SPI2,
        spi::master::Config::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(spi::Mode::_0),
    )
        .unwrap()
        .with_sck(peripherals.GPIO12)
        .with_mosi(peripherals.GPIO11);

    let lcd_chip_select = Output::new(peripherals.GPIO10, Level::High, OutputConfig::default());
    let lcd_spi_device = ExclusiveDevice::new_no_delay(lcd_spi, lcd_chip_select).unwrap();

    // data buffer for screen output (pixel buffers are [u16; SCREEN_WIDTH * SCREEN_HEIGHT])
    let mut mipi_buffer = [0_u8; 2 * SCREEN_WIDTH * SCREEN_HEIGHT];

    let lcd_data = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());
    let spi_interface = SpiInterface::new(lcd_spi_device, lcd_data, &mut mipi_buffer);

    let mut display = mipidsi::Builder::new(ST7735s, spi_interface)
        .reset_pin(
            Output::new(
                peripherals.GPIO7,
                Level::High,
                OutputConfig::default().with_drive_mode(DriveMode::OpenDrain),
            )
        )
        .orientation(
            mipidsi::options::Orientation::new()
                .flip_vertical()
                .flip_horizontal(),
        )
        .init(&mut delay)
        .unwrap();

    display.clear(Rgb565::BLACK).unwrap();

    // frame buffers
    let mut grid_buf: ConwayFramebuffer = Framebuffer::new();
    let mut display_buf: ConwayFramebuffer;

    // game state
    let mut generation_count = 0;
    let mut grid: ConwayGrid = [[false; GRID_WIDTH]; GRID_HEIGHT];
    let mut two_gens_ago: ConwayGrid = grid;

    let mut surviving_cell_color = Rgb565::GREEN;
    let mut stuck = false;

    // start game
    randomize_grid(&mut rng, &mut grid);
    backlight.set_high();

    info!("Hello, Conway!");

    loop {
        if !stuck {
            generation_count += 1;

            // stable patterns are either static or flicker between two states,
            // so the grid is "stuck" if every other generation is identical
            if generation_count % 2 == 0 {
                if grid.eq(&two_gens_ago) {
                    stuck = true;
                    surviving_cell_color = Rgb565::RED;
                }
                else {
                    two_gens_ago = grid;
                }
            }
        }

        // next generation to grid buffer
        grid = update_game_of_life(grid);
        draw_grid(&mut grid_buf, &grid, surviving_cell_color);

        // grid buffer + text to display buffer
        display_buf = grid_buf.clone();
        write_num(&mut display_buf, generation_count, 1).unwrap();
        write_text(&mut display_buf, "hello, CCC!", 3).unwrap();

        // display buffer to screen
        display_buf.as_image().draw(&mut display).unwrap();

        delay.delay_millis(FRAMEDELAY_MS);
    }
}
