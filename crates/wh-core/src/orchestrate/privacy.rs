//! Keeping other apps out of the pictures.
//!
//! Every picture and every OCR read leaves the machine: the model runs in its provider's cloud.
//! Until 2026-10-29 a screenshot was the whole display, so a task limited to TextEdit and a test
//! browser still sent the owner's Gmail tab, chat previews in notification banners and anything in
//! a password manager. Now the pixels of those surfaces are painted over before a capture is used
//! for anything:
//!
//! - always: password managers and the processes the owner listed in `protected.txt`;
//! - under `only_apps`: every app the task may not touch, except the menu bar, Dock and other
//!   parts of the system a person uses to get around.
//!
//! Cost: one window-server list per capture (about 1 ms on the 2026 Mac) and, only when something
//! must be hidden, a fill of the hidden rectangles (about 2 ms for a full 1920x1080 frame).

use crate::backend::{Image, Rect, ScreenLayer};

use super::Ctx;

/// Apps whose windows show secrets in plain text: never in a picture, whatever the task allows.
/// Bundle ids (macOS) and executable names (Windows), lower case.
const ALWAYS_HIDDEN: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.agilebits.onepassword-osx",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.passwords",
    "org.keepassxc.keepassxc",
    "com.lastpass.lastpassmacdesktop",
    "com.dashlane.dashlanephonefinal",
    "in.sinew.enpass-desktop",
    "1password.exe",
    "bitwarden.exe",
    "keepassxc.exe",
    "keepass.exe",
    "lastpass.exe",
    "dashlane.exe",
    "enpass.exe",
];

/// System surfaces a person uses to get around: shown even under `only_apps`. An empty app is the
/// window server itself (the menu bar background, the cursor).
const SYSTEM_SHOWN: &[&str] = &[
    "",
    "com.apple.dock",
    "com.apple.systemuiserver",
    "com.apple.controlcenter",
    "com.apple.windowmanager",
    "com.apple.wallpaper.agent",
];

/// What a hidden surface is painted with: dark grey, so it reads as "something is here" rather than
/// as a black window the model might try to wake up.
const HIDDEN_RGBA: [u8; 4] = [48, 48, 48, 255];

/// Paint `hide` rectangles (topmost first, `true` = hide) over `img`, a capture whose top-left is
/// `img.origin()` in input coordinates at `img.scale()` pixels per unit. Returns how many pixels were
/// painted. A front surface that is shown covers a hidden one behind it.
pub(crate) fn paint_hidden(img: Image, layers: &[(Rect, bool)]) -> (Image, usize) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (origin, scale) = (img.origin(), img.scale());
    let to_px = |r: &Rect| -> Option<(usize, usize, usize, usize)> {
        let px = |v: i32, o: i32, max: usize| {
            ((f64::from(v - o) * scale).round().max(0.0) as usize).min(max)
        };
        let (l, t) = (px(r.left, origin.x, w), px(r.top, origin.y, h));
        let (rr, b) = (px(r.right, origin.x, w), px(r.bottom, origin.y, h));
        (rr > l && b > t).then_some((l, t, rr, b))
    };
    if !layers.iter().any(|(r, hide)| *hide && to_px(r).is_some()) {
        return (img, 0);
    }
    // owner per pixel, painted back to front so the topmost surface wins: 0 none, 1 shown, 2 hidden
    let mut owner = vec![0u8; w * h];
    for (r, hide) in layers.iter().rev() {
        let Some((l, t, rr, b)) = to_px(r) else {
            continue;
        };
        let v = if *hide { 2 } else { 1 };
        for y in t..b {
            owner[y * w + l..y * w + rr].fill(v);
        }
    }
    let scale_out = img.scale();
    let mut rgba = img.into_rgba();
    let mut painted = 0;
    for (px, o) in rgba.chunks_exact_mut(4).zip(&owner) {
        if *o == 2 {
            px.copy_from_slice(&HIDDEN_RGBA);
            painted += 1;
        }
    }
    let out = Image::new(w as u32, h as u32, rgba)
        .expect("same size")
        .with_scale(scale_out)
        .with_origin(origin);
    (out, painted)
}

impl Ctx<'_> {
    /// Whether a surface of `app` titled `title` stays out of this call's pictures.
    pub(crate) fn hidden_in_pictures(&self, app: &str, title: &str) -> bool {
        let name = app.to_lowercase();
        if ALWAYS_HIDDEN.contains(&name.as_str()) {
            return true;
        }
        if self.o.only_apps.is_empty() || SYSTEM_SHOWN.contains(&name.as_str()) {
            return false;
        }
        if super::guard::SHELL.contains(&name.as_str())
            || self.o.only_apps.contains(&name)
            || self.allows_bundle_name(&name)
        {
            return false;
        }
        // a title-scoped entry ("com.apple.textedit:untitled") shows only the windows it names
        let title = title.to_lowercase();
        !self
            .o
            .only_apps
            .iter()
            .filter_map(|a| a.split_once(':'))
            .filter(|(proc_part, _)| super::guard::entry_names_app(proc_part, &name))
            .any(|(_, part)| {
                if part.starts_with("re:") {
                    regex::Regex::new(&format!("(?i){}", &part[3..]))
                        .is_ok_and(|re| re.is_match(&title))
                } else {
                    title.contains(&part.to_lowercase())
                }
            })
    }

    /// `img` with every surface this call may not see painted over. A capture whose layers cannot be
    /// listed is returned as it is only when no allowlist or protection applies; otherwise it is
    /// painted over entirely, because a failed list must not leak what it would have hidden.
    pub(crate) fn hide_private(&self, img: Image) -> Image {
        // a model on this machine keeps what it sees here; skip the work
        if self.cfg().local_model() {
            return img;
        }
        let protected = self.load_protected();
        let layers: Vec<ScreenLayer> = match self.be().screen_layers(!self.o.only_apps.is_empty()) {
            Ok(l) => l,
            Err(_) if self.o.only_apps.is_empty() && protected.is_empty() => return img,
            Err(_) => {
                let o = img.origin();
                let units = |px: u32| (f64::from(px) / img.scale().max(0.01)).ceil() as i32 + 1;
                let all = Rect::from_xywh(o.x, o.y, units(img.width()), units(img.height()));
                return paint_hidden(img, &[(all, true)]).0;
            }
        };
        let marks: Vec<(Rect, bool)> = layers
            .iter()
            .map(|l| {
                let hide = self.hidden_in_pictures(&l.app, &l.title)
                    || protected.contains(&l.app.to_lowercase());
                (l.rect, hide)
            })
            .collect();
        paint_hidden(img, &marks).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Point;

    fn img(w: u32, h: u32) -> Image {
        Image::solid(w, h, [200, 200, 200, 255]).with_origin(Point::new(0, 0))
    }

    fn at(i: &Image, x: usize, y: usize) -> [u8; 4] {
        let o = (y * i.width() as usize + x) * 4;
        i.rgba()[o..o + 4].try_into().unwrap()
    }

    #[test]
    fn a_hidden_window_is_painted_and_a_shown_one_in_front_of_it_is_not() {
        // topmost first: a shown window overlapping the left half of a hidden one
        let layers = [
            (Rect::from_xywh(0, 0, 40, 40), false),
            (Rect::from_xywh(20, 0, 60, 40), true),
        ];
        let (out, painted) = paint_hidden(img(100, 50), &layers);
        assert_eq!(
            at(&out, 30, 10),
            [200, 200, 200, 255],
            "shown window in front"
        );
        assert_eq!(at(&out, 60, 10), HIDDEN_RGBA, "hidden window");
        assert_eq!(at(&out, 90, 45), [200, 200, 200, 255], "desktop");
        assert_eq!(painted, 40 * 40);
    }

    #[test]
    fn nothing_to_hide_returns_the_picture_untouched_and_retina_pixels_are_scaled() {
        let (out, painted) = paint_hidden(img(100, 50), &[(Rect::from_xywh(0, 0, 10, 10), false)]);
        assert_eq!(painted, 0);
        assert_eq!(at(&out, 5, 5), [200, 200, 200, 255]);
        // a 2x capture of the area from (100, 100): the hidden 10x10 points are 20x20 pixels
        let retina = img(100, 50)
            .with_scale(2.0)
            .with_origin(Point::new(100, 100));
        let (out, painted) = paint_hidden(retina, &[(Rect::from_xywh(110, 110, 10, 10), true)]);
        assert_eq!(painted, 400);
        assert_eq!(at(&out, 25, 25), HIDDEN_RGBA);
        assert_eq!(at(&out, 45, 25), [200, 200, 200, 255]);
    }
}
