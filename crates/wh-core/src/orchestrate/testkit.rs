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
