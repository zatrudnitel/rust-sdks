#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality { Low, Medium, High }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EncodeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate_kbps: u32,
}

pub fn encode_config(q: Quality) -> EncodeConfig {
    match q {
        Quality::Low    => EncodeConfig { width: 1280, height: 720,  fps: 30, max_bitrate_kbps: 2500 },
        Quality::Medium => EncodeConfig { width: 1600, height: 900,  fps: 60, max_bitrate_kbps: 5000 },
        Quality::High   => EncodeConfig { width: 1920, height: 1080, fps: 60, max_bitrate_kbps: 8000 },
    }
}

impl std::str::FromStr for Quality {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Quality::Low),
            "medium" => Ok(Quality::Medium),
            "high" => Ok(Quality::High),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rungs_match_the_spec() {
        assert_eq!(encode_config(Quality::Low),    EncodeConfig { width: 1280, height: 720,  fps: 30, max_bitrate_kbps: 2500 });
        assert_eq!(encode_config(Quality::Medium), EncodeConfig { width: 1600, height: 900,  fps: 60, max_bitrate_kbps: 5000 });
        assert_eq!(encode_config(Quality::High),   EncodeConfig { width: 1920, height: 1080, fps: 60, max_bitrate_kbps: 8000 });
    }

    #[test]
    fn dimensions_are_always_even() {
        for q in [Quality::Low, Quality::Medium, Quality::High] {
            let c = encode_config(q);
            assert_eq!(c.width % 2, 0);
            assert_eq!(c.height % 2, 0);
        }
    }

    #[test]
    fn parse_quality() {
        assert_eq!("low".parse::<Quality>().unwrap(), Quality::Low);
        assert_eq!("HIGH".parse::<Quality>().unwrap(), Quality::High);
        assert!("ultra".parse::<Quality>().is_err());
    }
}
