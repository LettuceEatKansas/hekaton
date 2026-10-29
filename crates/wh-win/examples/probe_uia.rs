//! Times one Explorer window's UIA read under different query shapes.
//! Windows only: on other systems it compiles to a stub so `cargo test` builds everywhere.

#[cfg(windows)]
mod imp {
    use std::time::Instant;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Accessibility::*;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::w;

    pub fn main() -> windows::core::Result<()> {
        // SAFETY: a probe on this thread's own COM objects; every call checks its result.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let hwnd = FindWindowW(w!("CabinetWClass"), None)?;
            let all = [
                UIA_NamePropertyId,
                UIA_ControlTypePropertyId,
                UIA_IsEnabledPropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_IsOffscreenPropertyId,
                UIA_IsPasswordPropertyId,
                UIA_HasKeyboardFocusPropertyId,
                UIA_IsInvokePatternAvailablePropertyId,
                UIA_IsValuePatternAvailablePropertyId,
                UIA_IsTogglePatternAvailablePropertyId,
                UIA_IsSelectionItemPatternAvailablePropertyId,
                UIA_IsExpandCollapsePatternAvailablePropertyId,
                UIA_IsTextPatternAvailablePropertyId,
                UIA_ValueValuePropertyId,
                UIA_ToggleToggleStatePropertyId,
                UIA_SelectionItemIsSelectedPropertyId,
                UIA_ExpandCollapseExpandCollapseStatePropertyId,
            ];
            let make = |props: &[UIA_PROPERTY_ID],
                        none_mode: bool|
             -> windows::core::Result<IUIAutomationCacheRequest> {
                let c = auto.CreateCacheRequest()?;
                for p in props {
                    c.AddProperty(*p)?;
                }
                if none_mode {
                    c.SetAutomationElementMode(AutomationElementMode_None)?;
                }
                Ok(c)
            };
            let full = make(&all, false)?;
            let full_none = make(&all, true)?;
            let small = make(&all[..5], false)?;
            let truec = auto.CreateTrueCondition()?;
            let ctrl = auto.ControlViewCondition()?;
            let notoff = auto.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &false.into())?;
            let ctrl_on = auto.CreateAndCondition(&ctrl, &notoff)?;
            let root = auto.ElementFromHandle(hwnd)?;
            let cases: [(&str, &IUIAutomationCondition, &IUIAutomationCacheRequest); 6] = [
                ("true+17", &truec, &full),
                ("true+17 none-mode", &truec, &full_none),
                ("true+5", &truec, &small),
                ("control+17", &ctrl, &full),
                ("control+onscreen+17", &ctrl_on, &full),
                ("control+onscreen+17 none", &ctrl_on, &full_none),
            ];
            let doc = auto.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &(UIA_DocumentControlTypeId.0).into(),
            )?;
            let doc_on = auto.CreateAndCondition(&notoff, &doc)?;
            for round in 0..3 {
                for (name, cond) in [("doc", &doc), ("onscreen&doc", &doc_on)] {
                    let t = Instant::now();
                    let found = root.FindAll(TreeScope_Descendants, cond)?;
                    println!(
                        "r{round} {name:28} n={:4} {:6.1} ms",
                        found.Length()?,
                        t.elapsed().as_secs_f64() * 1e3
                    );
                }
                for (name, cond, cache) in &cases {
                    let t = Instant::now();
                    let found = root.FindAllBuildCache(TreeScope_Descendants, *cond, *cache)?;
                    println!(
                        "r{round} {name:28} n={:4} {:6.1} ms",
                        found.Length()?,
                        t.elapsed().as_secs_f64() * 1e3
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    imp::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("probe_uia runs on Windows only");
}
