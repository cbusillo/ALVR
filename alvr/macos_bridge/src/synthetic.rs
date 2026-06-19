pub struct SyntheticFrameSource {
    width: usize,
    height: usize,
    eye_width: usize,
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl SyntheticFrameSource {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width as usize;
        let height = height as usize;
        let eye_width = width / 2;
        let y_size = width * height;
        let uv_size = width * height.div_ceil(2);

        Self {
            width,
            height,
            eye_width,
            y: vec![0; y_size],
            uv: vec![128; uv_size],
        }
    }

    pub fn frame(&mut self, frame_index: u64) -> (&[u8], &[u8]) {
        let drift = (frame_index % 256) as usize;

        for y in 0..self.height {
            let row_start = y * self.width;
            for x in 0..self.width {
                self.y[row_start + x] = self.luma_for_pixel(x, y, drift, frame_index);
            }
        }

        let uv_width = self.width.div_ceil(2);
        let uv_height = self.height.div_ceil(2);
        for uv_y in 0..uv_height {
            for uv_x in 0..uv_width {
                let index = uv_y * self.width + uv_x * 2;
                let eye = usize::from(uv_x >= self.eye_width / 2);
                let local_uv_x = if eye == 0 {
                    uv_x
                } else {
                    uv_x - self.eye_width / 2
                };

                let chroma_pulse = ((local_uv_x + uv_y + drift / 2) % 12) as u8;
                self.uv[index] = 128 + chroma_pulse;
                self.uv[index + 1] = 128 - chroma_pulse;
            }
        }

        (&self.y, &self.uv)
    }

    fn luma_for_pixel(&self, x: usize, y: usize, drift: usize, frame_index: u64) -> u8 {
        let eye = usize::from(x >= self.eye_width);
        let local_x = if eye == 0 { x } else { x - self.eye_width };
        if local_x == 0 || local_x + 1 == self.eye_width || y == 0 || y + 1 == self.height {
            return 235;
        }

        if local_x % 160 < 2 || y % 160 < 2 {
            return 210;
        }

        if is_left_marker(eye, local_x, y) || is_right_marker(eye, local_x, y) {
            return 235;
        }

        let center_x = self.eye_width / 2;
        let center_y = self.height / 2;
        let moving_x = center_x + ((drift * 3) % 96) - 48;

        if local_x.abs_diff(center_x) < 3 || y.abs_diff(center_y) < 3 {
            return 28;
        }

        if local_x.abs_diff(moving_x) < 24 && y.abs_diff(center_y) < 42 {
            return 235;
        }

        let checker = ((local_x / 64) + (y / 64) + (frame_index as usize / 12)) % 2;
        let gradient = (local_x + y + drift) % 160;
        if checker == 0 {
            (48 + gradient) as u8
        } else {
            (200 - gradient / 2) as u8
        }
    }
}

fn is_left_marker(eye: usize, x: usize, y: usize) -> bool {
    if eye != 0 || !(32..92).contains(&x) || !(32..112).contains(&y) {
        return false;
    }

    (32..46).contains(&x) || (98..112).contains(&y)
}

fn is_right_marker(eye: usize, x: usize, y: usize) -> bool {
    if eye != 1 || !(32..98).contains(&x) || !(32..112).contains(&y) {
        return false;
    }

    let in_stem = (32..46).contains(&x);
    let in_top = (32..46).contains(&y) && (32..86).contains(&x);
    let in_mid = (66..80).contains(&y) && (32..86).contains(&x);
    let in_bowl = (78..92).contains(&x) && (32..80).contains(&y);
    let in_leg = x > 54 && y > 76 && x.abs_diff(y - 22) < 10;

    in_stem || in_top || in_mid || in_bowl || in_leg
}
