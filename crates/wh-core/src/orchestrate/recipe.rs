//! The batch-loop side of recipes: what `run_many` does around a step that came from a recipe or
//! carries `when` or `on_fail`. Port of `recipes.skip_reason`, `recipes.scope` and the `recipe_list`,
//! `recipe_save` and `recipe` branches of `recipes.run`. The data side (format, validation, binding,
//! expansion, saving) is [`crate::recipes`].

use std::collections::BTreeSet;

use super::actions::StepOut;
use super::args::Args;
use super::error::{StepError, StepResult};
use super::{CallOpts, Ctx};

impl Ctx<'_> {
    /// `""` to run the step; else why its `when` says not to. A malformed `when` fails the step.
    /// A `when` is the same check as `expect`, evaluated before the step instead of after it.
    /// Python `recipes.skip_reason`.
    pub(crate) fn skip_reason(&self, step: &Args) -> StepResult<String> {
        let Some(when) = step.get("when").filter(|v| !v.is_null()) else {
            return Ok(String::new());
        };
        crate::recipes::check_when(when, "when").map_err(StepError::value)?;
        let verdict = self.check_expect(when)?;
        if verdict == "expect: PASS" {
            return Ok(String::new());
        }
        let why = match verdict.strip_prefix("expect: FAIL (") {
            Some(rest) => rest.strip_suffix(')').unwrap_or(rest),
            None => verdict.as_str(),
        };
        Ok(format!("when not met ({why})"))
    }

    /// Run one step under its recipe's allowlist (which only ever narrows the call's), or under the
    /// call's own when it has none. The call's allowlist is not touched: the step gets a context of
    /// its own, so it is back in force the moment the step ends, however it ends. A `scope` on the
    /// step (not the call's own action, `call_args`) is set for this step only, under that allowlist.
    /// Python `recipes.scope` with `_step_scope`.
    pub(crate) fn run_scoped(
        &self,
        step: &Args,
        scope: Option<&BTreeSet<String>>,
        call_args: bool,
    ) -> StepResult<StepOut> {
        match scope {
            None => self.run_step_scope(step, call_args),
            Some(only_apps) => {
                let narrowed = Ctx {
                    c: self.c,
                    o: CallOpts {
                        only_apps: only_apps.clone(),
                        ..self.o.clone()
                    },
                    who: self.who,
                };
                narrowed.run_step_scope(step, call_args)
            }
        }
    }

    /// `recipe_list`: one cheap line per recipe.
    pub(crate) fn recipe_list(&self) -> StepResult<StepOut> {
        Ok((self.c.recipes.list_text(), None))
    }

    /// `recipe_save`: write a recipe to the owner's folder.
    pub(crate) fn recipe_save(&self, step: &Args) -> StepResult<StepOut> {
        let note = crate::recipes::save(&self.c.recipes, step).map_err(StepError::value)?;
        Ok((note, None))
    }

    /// `recipe` outside `run_many` (there is nothing to expand it): explain how to call it.
    pub(crate) fn recipe_direct(&self) -> StepResult<StepOut> {
        Err(StepError::value(
            "a recipe runs inside a batch (computer.run_many expands it); send it as a normal \
             computer call: action=recipe, name=..., params={...}",
        ))
    }
}
