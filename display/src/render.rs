use crate::fb::Framebuffer;
use crate::{FB_WIDTH, FB_HEIGHT};
use embedded_graphics_core::{
    draw_target::DrawTarget,
    pixelcolor::BinaryColor,
    prelude::{OriginDimensions, Size},
    Pixel,
};

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(FB_WIDTH as u32, FB_HEIGHT as u32)
    }
}

impl DrawTarget for Framebuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0
                && coord.x < (FB_WIDTH as i32)
                && coord.y >= 0
                && coord.y < (FB_HEIGHT as i32)
            {
                let white = match color {
                    BinaryColor::On => true,
                    BinaryColor::Off => false,
                };
                self.set_pixel(coord.x as usize, coord.y as usize, white);
            }
        }
        Ok(())
    }
}

/// Bresenham's line algorithm for fast flat line drawing.
pub fn draw_line(fb: &mut Framebuffer, mut x0: i32, mut y0: i32, x1: i32, y1: i32, white: bool) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        if x0 >= 0 && x0 < (FB_WIDTH as i32) && y0 >= 0 && y0 < (FB_HEIGHT as i32) {
            fb.set_pixel(x0 as usize, y0 as usize, white);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// Helper for drawing a filled or wireframe rectangle.
pub fn draw_rect(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    fill: bool,
    white: bool,
) {
    if w == 0 || h == 0 {
        return;
    }
    if fill {
        for curr_y in y..y + h {
            for curr_x in x..x + w {
                fb.set_pixel(curr_x, curr_y, white);
            }
        }
    } else {
        // Wireframe
        let x1 = (x + w - 1) as i32;
        let y1 = (y + h - 1) as i32;
        let xi = x as i32;
        let yi = y as i32;
        draw_line(fb, xi, yi, x1, yi, white);
        draw_line(fb, x1, yi, x1, y1, white);
        draw_line(fb, x1, y1, xi, y1, white);
        draw_line(fb, xi, y1, xi, yi, white);
    }
}
