//! A desktop in a box for the orchestration tests: a [`FakeBackend`] with one small non-blank display,
//! a [`FakeClock`] so waits cost nothing, and a [`Computer`] over them. The owner has acknowledged
//! winhands, so acting actions run.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{Args, CallOpts, Computer, Ctx, ManyResult, StepError};
use crate::backend::fake::FakeBackend;
use crate::backend::{Display, Image, Rect, WindowInfo, WindowRef};
use crate::caller::Caller;
use crate::clock::FakeClock;
use crate::config::Config;
use crate::destructive::Policy;
use crate::env::{Env, MapEnv};
use crate::paths::Paths;
use crate::testutil::TempDir;

/// The width and height of the kit's display.
pub(crate) const W: i32 = 320;
/// See [`W`].
pub(crate) const H: i32 = 180;

/// A non-blank picture: not one flat colour (a flat screen reads as "loading" to the settle loop).
pub(crate) fn textured(w: u32, h: u32) -> Image {
    let mut bytes = Vec::new();
    for i in 0..(w * h) {
        let v = ((i / 2) * 7 % 256) as u8;
        bytes.extend_from_slice(&[v, v, v, 255]);
    }
    Image::new(w, h, bytes).expect("textured image")
}

/// Width and height from the IHDR chunk of a base64 PNG.
pub(crate) fn png_size(b64: &str) -> (u32, u32) {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64");
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (w, h)
}

pub(crate) struct Kit {
    pub fake: Arc<FakeBackend>,
    pub clock: Arc<FakeClock>,
    pub computer: Computer,
    pub who: Caller,
    pub tmp: TempDir,
}

impl Kit {
    pub fn new() -> Self {
        Self::with_env(MapEnv::new())
    }

    pub fn with_env(env: MapEnv) -> Self {
        let tmp = TempDir::new("orch");
        let mut env = env;
        // the owner has acknowledged unless a test says otherwise
        if env.var("WINHANDS_ACKNOWLEDGED").is_none() {
            env.set("WINHANDS_ACKNOWLEDGED", "1");
        }
        if env.var("TEMP").is_none() {
            env.set("TEMP", &tmp.path().to_string_lossy());
        }
        let fake = Arc::new(FakeBackend::new());
        fake.set_displays(vec![Display {
            id: 1,
            name: "kit".into(),
            rect: Rect::from_xywh(0, 0, W, H),
            scale: 1.0,
            primary: true,
        }]);
        fake.set_screen(textured(W as u32, H as u32));
        let clock = Arc::new(FakeClock::new());
        let cfg = Arc::new(Config::new(Paths::in_dir(tmp.path().join("data")), env));
        let computer = Computer::new(fake.clone(), clock.clone(), cfg);
        Self {
            fake,
            clock,
            computer,
            who: Caller::unknown(),
            tmp,
        }
    }

    /// Add a visible window and return its handle.
    pub fn window(&self, title: &str, app: &str, pid: u32, rect: Rect) -> WindowRef {
        self.fake
            .add_window(WindowInfo::basic(title, app, pid, rect))
    }

    /// Add a window and put it in front.
    pub fn front(&self, title: &str, app: &str, pid: u32) -> WindowRef {
        let w = self.window(title, app, pid, Rect::from_xywh(10, 10, W - 20, H - 20));
        self.fake.set_foreground(Some(w));
        w
    }

    /// The owner's recipe folder of this kit.
    pub fn user_recipes(&self) -> std::path::PathBuf {
        self.computer.cfg.paths().recipes_dir()
    }

    /// Start from a store with no bundled recipes at all (the owner's folder is still this kit's).
    pub fn empty_recipes(&mut self) {
        self.computer.recipes =
            crate::recipes::RecipeStore::with_bundled(self.user_recipes(), vec![]);
    }

    /// Add a bundled recipe (its `name` is the file stem).
    pub fn put_bundled(&mut self, rec: &Value) {
        let name = rec["name"].as_str().expect("recipe name").to_string();
        self.computer.recipes.put_bundled(&name, &rec.to_string());
    }

    /// Write one of the owner's recipes to disk.
    pub fn put_user(&self, rec: &Value) {
        let name = rec["name"].as_str().expect("recipe name");
        std::fs::create_dir_all(self.user_recipes()).expect("recipe folder");
        std::fs::write(
            self.user_recipes().join(format!("{name}.json")),
            rec.to_string(),
        )
        .expect("write recipe");
    }

    /// A call's arguments from JSON.
    pub fn args(v: Value) -> Args {
        v.as_object().cloned().expect("an object")
    }

    /// `run_many` with the destructive refusals on and an unidentified caller.
    pub fn run(&self, v: Value) -> Result<ManyResult, StepError> {
        self.computer
            .run_many(&Self::args(v), Policy::REFUSE, &self.who)
    }

    /// A call's context with these arguments, for the tests of single helpers.
    pub fn ctx(&self, v: Value) -> Ctx<'_> {
        let o = CallOpts::from_args(&self.computer, &Self::args(v), Policy::REFUSE).expect("opts");
        Ctx {
            c: &self.computer,
            o,
            who: &self.who,
        }
    }

    /// One action through the real `run`, the way a batch calls it.
    pub fn step(&self, v: Value) -> Result<(String, Option<String>), StepError> {
        let ctx = self.ctx(json!({}));
        ctx.run(&Self::args(v))
    }

    /// One action under an allowlist.
    pub fn step_only(
        &self,
        only: &[&str],
        v: Value,
    ) -> Result<(String, Option<String>), StepError> {
        let ctx = self.ctx(json!({ "only_apps": only }));
        ctx.run(&Self::args(v))
    }
}

/// The key the core presses for select-all, copy, paste and new-tab on this OS: Ctrl on Windows,
/// Command on a Mac. (A step that says `ctrl+c` still presses Ctrl everywhere.)
pub(crate) fn cmd_key() -> crate::keys::Key {
    match crate::keys::Platform::current() {
        crate::keys::Platform::Windows => crate::keys::Key::Ctrl,
        crate::keys::Platform::Mac => crate::keys::Key::Meta,
    }
}
