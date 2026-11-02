//! Theme-tinted monochrome V mark for desktop / system menu icons.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use cairo::{Context, Format, ImageSurface};

use super::theme::Theme;

/// Icon theme name (also used by .desktop).
pub const ICON_NAME: &str = "omakeys";

/// Left / right arms of the prototype V (16x16 viewBox).
const V_LEFT: &str = "M7.23,0.25L1.54,4.48L0.25,5.89L0.25,15.52L4.12,11.52L4.12,8.70L7.23,3.77Z";
const V_RIGHT: &str = "M8.52,0.25L14.21,4.48L15.50,5.89L15.50,15.52L11.63,11.52L11.63,8.70L8.52,3.77Z";

fn svg_for_fg(fg: &str) -> String {
    let fg = fg.trim();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
  <path fill="{fg}" d="{V_LEFT}"/>
  <path fill="{fg}" d="{V_RIGHT}"/>
</svg>
"#
    )
}

fn parse_hex_rgb(fg: &str) -> (f64, f64, f64) {
    let h = fg.trim().trim_start_matches('#');
    if h.len() >= 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0xe2) as f64 / 255.0;
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0xcf) as f64 / 255.0;
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0xc9) as f64 / 255.0;
        (r, g, b)
    } else {
        (0.89, 0.81, 0.79)
    }
}

fn parse_svg_path(d: &str) -> Vec<(f64, f64)> {
    let mut pts = Vec::new();
    for part in d.split(|c| c == 'M' || c == 'L' || c == 'Z' || c == 'z') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut nums = part
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty());
        if let (Some(x), Some(y)) = (nums.next(), nums.next()) {
            if let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) {
                pts.push((x, y));
            }
        }
    }
    pts
}

fn append_path(cr: &Context, d: &str) {
    let pts = parse_svg_path(d);
    if pts.is_empty() {
        return;
    }
    cr.new_path();
    cr.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        cr.line_to(p.0, p.1);
    }
    cr.close_path();
}

fn icon_dirs() -> (PathBuf, PathBuf) {
    let base = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("icons/hicolor");
    (
        base.join("scalable/apps"),
        base.join("symbolic/apps"),
    )
}

const HICOLOR_INDEX: &str = "\
[Icon Theme]
Name=Hicolor
Comment=Fallback icon theme
Hidden=true
Directories=16x16/apps,22x22/apps,24x24/apps,32x32/apps,48x48/apps,64x64/apps,128x128/apps,scalable/apps,symbolic/apps

[16x16/apps]
Size=16
Context=Applications
Type=Fixed

[22x22/apps]
Size=22
Context=Applications
Type=Fixed

[24x24/apps]
Size=24
Context=Applications
Type=Fixed

[32x32/apps]
Size=32
Context=Applications
Type=Fixed

[48x48/apps]
Size=48
Context=Applications
Type=Fixed

[64x64/apps]
Size=64
Context=Applications
Type=Fixed

[128x128/apps]
Size=128
Context=Applications
Type=Fixed

[scalable/apps]
Size=128
Context=Applications
Type=Scalable
MinSize=1
MaxSize=256

[symbolic/apps]
Size=16
Context=Applications
Type=Scalable
MinSize=1
MaxSize=256
";

fn write_png_sizes(hicolor: &std::path::Path, fg: &str) -> anyhow::Result<()> {
    let (r, g, b) = parse_hex_rgb(fg);
    for size in [16, 22, 24, 32, 48, 64, 128] {
        let dir = hicolor.join(format!("{size}x{size}/apps"));
        fs::create_dir_all(&dir)?;
        let surface = ImageSurface::create(Format::ARgb32, size, size)?;
        {
            let cr = Context::new(&surface)?;
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.set_operator(cairo::Operator::Source);
            cr.paint()?;
            cr.set_operator(cairo::Operator::Over);
            let scale = f64::from(size) / 16.0;
            cr.scale(scale, scale);
            cr.set_source_rgb(r, g, b);
            append_path(&cr, V_LEFT);
            cr.fill()?;
            append_path(&cr, V_RIGHT);
            cr.fill()?;
        }
        let path = dir.join(format!("{ICON_NAME}.png"));
        let mut file = fs::File::create(&path)?;
        surface.write_to_png(&mut file)?;
    }
    Ok(())
}

/// Write theme-colored V icons so system menu matches Omarchy foreground.
pub fn apply_theme_icons(theme: &Theme) -> anyhow::Result<()> {
    let (scalable, symbolic) = icon_dirs();
    fs::create_dir_all(&scalable)?;
    fs::create_dir_all(&symbolic)?;
    let svg = svg_for_fg(&theme.foreground);
    fs::write(scalable.join(format!("{ICON_NAME}.svg")), &svg)?;
    fs::write(symbolic.join(format!("{ICON_NAME}-symbolic.svg")), &svg)?;

    let hicolor = scalable
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    if let Some(dir) = hicolor {
        // Empty Directories= breaks theme lookup.
        fs::write(dir.join("index.theme"), HICOLOR_INDEX)?;
        write_png_sizes(&dir, &theme.foreground)?;
        let _ = Command::new("gtk-update-icon-cache")
            .args(["-f", &dir.to_string_lossy()])
            .output();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_uses_theme_fg() {
        let s = svg_for_fg("#E2CFC9");
        assert!(s.contains("fill=\"#E2CFC9\""));
        assert!(!s.contains("currentColor"));
        assert!(!s.contains("keySide"));
    }
}
