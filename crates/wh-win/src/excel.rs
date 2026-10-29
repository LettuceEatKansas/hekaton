//! The Excel power channel on Windows: a hidden Excel this backend starts itself, driven through
//! its automation interface (`IDispatch`, late bound, the way a script drives it). Handing Excel a
//! whole block of cells in one call is orders of magnitude faster than typing into its window.
//!
//! Every COM object lives on ONE worker thread in the multithreaded apartment ([`Host`] starts it on
//! the first `new` or `open` and talks to it over a channel), so no interface pointer is ever shared
//! between threads and a slow Excel only holds up Excel calls. The instance is always a new process
//! of our own: `CoCreateInstance` is checked against the Excel processes that were running before,
//! and a hit means Windows handed back the person's Excel, which is released untouched and refused.
//! Macros are disabled on open. Charts and pivot tables are not supported yet.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use wh_core::backend::excel::{Area, parse_area, parse_cell};
use wh_core::backend::{BackendError, BackendResult, ExcelCell, ExcelReply, ExcelRequest};
use windows::Win32::Foundation::{DISP_E_EXCEPTION, DISP_E_PARAMNOTFOUND, HWND};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, CLSIDFromProgID, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT,
    DISPPARAMS, EXCEPINFO, IDispatch, SAFEARRAY, SAFEARRAYBOUND,
};
use windows::Win32::System::Ole::{
    DISPID_PROPERTYPUT, SafeArrayAccessData, SafeArrayCreate, SafeArrayGetDim, SafeArrayGetLBound,
    SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_BOOL, VT_BSTR, VT_DATE,
    VT_EMPTY, VT_ERROR, VT_I2, VT_I4, VT_NULL, VT_R4, VT_R8, VT_VARIANT,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{GUID, PCWSTR, w};

use crate::util::{ensure_com, wide};

/// `LOCALE_USER_DEFAULT`.
const LOCALE: u32 = 0x0400;
/// Excel's `xlOpenXMLWorkbook`, the `.xlsx` file format.
const XL_OPEN_XML_WORKBOOK: i32 = 51;
/// `msoAutomationSecurityForceDisable`: a workbook opened here never runs its macros.
const FORCE_DISABLE_MACROS: i32 = 3;
/// The longest one Excel call may take before the caller is told it timed out.
const CALL_LIMIT: Duration = Duration::from_secs(300);
/// How long shutdown waits for Excel to be told to quit.
const QUIT_WAIT: Duration = Duration::from_secs(20);

/// Is Excel installed (its automation class is registered)?
pub(crate) fn available() -> bool {
    ensure_com();
    // SAFETY: a registry lookup of a constant, NUL-terminated name.
    unsafe { CLSIDFromProgID(w!("Excel.Application")) }.is_ok()
}

// ---------------------------------------------------------------------------------------- host

enum Msg {
    Run(ExcelRequest, mpsc::Sender<BackendResult<ExcelReply>>),
    Quit(mpsc::Sender<()>),
}

struct Worker {
    tx: mpsc::Sender<Msg>,
    thread: JoinHandle<()>,
}

/// The owned Excel as the backend sees it: nothing runs until the first `new` or `open`.
#[derive(Default)]
pub(crate) struct Host {
    worker: Mutex<Option<Worker>>,
}

impl Host {
    /// Run one request on the worker, starting it for a `new` or `open`.
    pub(crate) fn run(&self, req: &ExcelRequest) -> BackendResult<ExcelReply> {
        let mut slot = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            if !matches!(req, ExcelRequest::New | ExcelRequest::Open { .. }) {
                return Err(no_instance());
            }
            let (tx, rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("wh-excel".into())
                .spawn(move || worker(&rx))
                .map_err(|e| BackendError::Os(format!("could not start the Excel thread: {e}")))?;
            *slot = Some(Worker { tx, thread });
        }
        let Some(worker) = slot.as_ref() else {
            return Err(no_instance());
        };
        let (tx, rx) = mpsc::channel();
        if worker.tx.send(Msg::Run(req.clone(), tx)).is_err() {
            *slot = None;
            return Err(BackendError::Os("the Excel thread has stopped".into()));
        }
        match rx.recv_timeout(CALL_LIMIT) {
            Ok(answer) => answer,
            Err(RecvTimeoutError::Timeout) => Err(BackendError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                *slot = None;
                Err(BackendError::Os("the Excel thread stopped mid-call".into()))
            }
        }
    }

    /// Quit the owned Excel, closing its workbooks without saving. Best effort.
    pub(crate) fn shutdown(&self) {
        let taken = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(worker) = taken else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        if worker.tx.send(Msg::Quit(tx)).is_ok() && rx.recv_timeout(QUIT_WAIT).is_ok() {
            let _ = worker.thread.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn no_instance() -> BackendError {
    BackendError::Os("no Excel instance; excel new or excel open starts one".into())
}

fn worker(rx: &mpsc::Receiver<Msg>) {
    // SAFETY: plain COM init on this thread, which only ever drives Excel; undone below.
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut session = Session::default();
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Run(req, reply) => {
                let _ = reply.send(session.handle(&req));
            }
            Msg::Quit(done) => {
                session.quit();
                let _ = done.send(());
                break;
            }
        }
    }
    // the channel closing without a Quit must not leave Excel running either
    session.quit();
    drop(session);
    if inited {
        // SAFETY: balances the CoInitializeEx above; every interface was released with `session`.
        unsafe { CoUninitialize() };
    }
}

// ----------------------------------------------------------------------------------- late binding

/// A failed Excel call: what to tell the caller, and whether Excel itself has gone away.
struct Fail {
    msg: String,
    gone: bool,
}

type XResult<T> = Result<T, Fail>;

impl From<Fail> for BackendError {
    fn from(f: Fail) -> Self {
        BackendError::Os(f.msg)
    }
}

fn fail(msg: impl Into<String>) -> Fail {
    Fail {
        msg: msg.into(),
        gone: false,
    }
}

/// `IDispatch::Invoke` of `name`, arguments given first to last. A property put names its value
/// the way automation wants. Returns the result variant.
fn invoke(
    target: &IDispatch,
    name: &str,
    flags: DISPATCH_FLAGS,
    mut args: Vec<VARIANT>,
) -> XResult<VARIANT> {
    let wname = wide(name);
    let names = [PCWSTR(wname.as_ptr())];
    let mut id = 0i32;
    // SAFETY: one NUL-terminated name in, one dispatch id out, both live for the call.
    unsafe { target.GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, LOCALE, &raw mut id) }
        .map_err(|e| fail(format!("Excel has no {name}: {e}")))?;
    // IDispatch takes its arguments last to first
    args.reverse();
    let mut put_id = DISPID_PROPERTYPUT;
    let mut params = DISPPARAMS {
        rgvarg: args.as_mut_ptr(),
        rgdispidNamedArgs: std::ptr::null_mut(),
        cArgs: args.len() as u32,
        cNamedArgs: 0,
    };
    if flags == DISPATCH_PROPERTYPUT {
        params.rgdispidNamedArgs = &raw mut put_id;
        params.cNamedArgs = 1;
    }
    let mut result = VARIANT::default();
    let mut excep = EXCEPINFO::default();
    let mut bad_arg = 0u32;
    // SAFETY: `params` points at `args`, which outlives the call; the result, exception record and
    // argument-error slot are locals that live for the call.
    let outcome = unsafe {
        target.Invoke(
            id,
            &GUID::zeroed(),
            LOCALE,
            flags,
            &raw const params,
            Some(&raw mut result),
            Some(&raw mut excep),
            Some(&raw mut bad_arg),
        )
    };
    // The exception record owns three strings that must be freed whether or not it was filled.
    let description = ManuallyDrop::into_inner(excep.bstrDescription).to_string();
    drop(ManuallyDrop::into_inner(excep.bstrSource));
    drop(ManuallyDrop::into_inner(excep.bstrHelpFile));
    match outcome {
        Ok(()) => Ok(result),
        Err(e) => {
            let code = e.code();
            // RPC_E_DISCONNECTED, RPC_S_SERVER_UNAVAILABLE, RPC_S_CALL_FAILED, RPC_S_CALL_FAILED_DNE
            let gone = matches!(
                code.0 as u32,
                0x8001_0108 | 0x8007_06BA | 0x8007_06BE | 0x8007_06BF
            );
            let why = if code == DISP_E_EXCEPTION && !description.is_empty() {
                description
            } else {
                e.message()
            };
            Err(Fail {
                msg: format!("Excel {name} failed: {why} (0x{:08X})", code.0 as u32),
                gone,
            })
        }
    }
}

const GET: DISPATCH_FLAGS = DISPATCH_FLAGS(DISPATCH_METHOD.0 | DISPATCH_PROPERTYGET.0);

fn get(target: &IDispatch, name: &str) -> XResult<VARIANT> {
    invoke(target, name, GET, Vec::new())
}

fn get_object(target: &IDispatch, name: &str) -> XResult<IDispatch> {
    object(&get(target, name)?, name)
}

fn put(target: &IDispatch, name: &str, value: VARIANT) -> XResult<()> {
    invoke(target, name, DISPATCH_PROPERTYPUT, vec![value]).map(|_| ())
}

fn method(target: &IDispatch, name: &str, args: Vec<VARIANT>) -> XResult<VARIANT> {
    invoke(target, name, GET, args)
}

fn object(v: &VARIANT, what: &str) -> XResult<IDispatch> {
    IDispatch::try_from(v).map_err(|_| fail(format!("Excel {what} did not return an object")))
}

/// An omitted optional argument.
fn missing() -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_ERROR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    scode: DISP_E_PARAMNOTFOUND.0,
                },
            }),
        },
    }
}

fn number_of(v: &VARIANT) -> Option<f64> {
    // SAFETY: each arm reads the union member that the variant's own type tag names.
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            VT_I4 => Some(f64::from(inner.Anonymous.lVal)),
            VT_I2 => Some(f64::from(inner.Anonymous.iVal)),
            VT_R8 => Some(inner.Anonymous.dblVal),
            VT_R4 => Some(f64::from(inner.Anonymous.fltVal)),
            _ => None,
        }
    }
}

fn text_of(v: &VARIANT) -> Option<String> {
    // SAFETY: the BSTR member is read only when the type tag says the variant holds one.
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        (inner.vt == VT_BSTR).then(|| String::from_utf16_lossy(&inner.Anonymous.bstrVal))
    }
}

/// Excel's error values as the cells show them.
fn error_text(scode: i32) -> String {
    match (scode as u32).wrapping_sub(0x800A_0000) {
        2000 => "#NULL!",
        2007 => "#DIV/0!",
        2015 => "#VALUE!",
        2023 => "#REF!",
        2029 => "#NAME?",
        2036 => "#NUM!",
        2042 => "#N/A",
        _ => "#ERROR!",
    }
    .to_string()
}

fn cell_of(v: &VARIANT) -> ExcelCell {
    // SAFETY: each arm reads the union member that the variant's own type tag names.
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            VT_EMPTY | VT_NULL => ExcelCell::Empty,
            VT_BSTR => ExcelCell::Text(String::from_utf16_lossy(&inner.Anonymous.bstrVal)),
            VT_BOOL => ExcelCell::Bool(inner.Anonymous.boolVal.0 != 0),
            VT_ERROR => ExcelCell::Text(error_text(inner.Anonymous.scode)),
            VT_DATE => ExcelCell::Number(inner.Anonymous.date),
            _ => number_of(v).map_or(ExcelCell::Empty, ExcelCell::Number),
        }
    }
}

fn variant_of(cell: &ExcelCell) -> VARIANT {
    match cell {
        ExcelCell::Empty => VARIANT::default(),
        ExcelCell::Text(t) => VARIANT::from(t.as_str()),
        ExcelCell::Number(n) => VARIANT::from(*n),
        ExcelCell::Bool(b) => VARIANT::from(*b),
    }
}

/// A block of cells as one two-dimensional array of variants, the shape `Range.Formula` takes.
/// A SAFEARRAY lays its first dimension out fastest, so the cell at row `r`, column `c` is slot
/// `r + c * rows`. The variant owns the array: dropping it frees every element too.
fn grid_variant(values: &[Vec<ExcelCell>]) -> XResult<VARIANT> {
    let rows = values.len();
    let cols = values.first().map_or(0, Vec::len);
    if rows == 1 && cols == 1 {
        return Ok(variant_of(&values[0][0]));
    }
    let bounds = [
        SAFEARRAYBOUND {
            cElements: rows as u32,
            lLbound: 1,
        },
        SAFEARRAYBOUND {
            cElements: cols as u32,
            lLbound: 1,
        },
    ];
    // SAFETY: two valid bounds for a two-dimensional array of variants, each slot starting empty.
    let psa = unsafe { SafeArrayCreate(VT_VARIANT, 2, bounds.as_ptr()) };
    if psa.is_null() {
        return Err(fail("out of memory building the array for Excel"));
    }
    let out = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VARENUM(VT_ARRAY.0 | VT_VARIANT.0),
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { parray: psa },
            }),
        },
    };
    let mut data: *mut c_void = std::ptr::null_mut();
    // SAFETY: `psa` is the live array just made; it stays locked only until the unaccess below.
    unsafe { SafeArrayAccessData(psa, &raw mut data) }
        .map_err(|e| fail(format!("could not fill the array for Excel: {e}")))?;
    let base = data.cast::<VARIANT>();
    for (r, line) in values.iter().enumerate() {
        for (c, cell) in line.iter().enumerate() {
            // SAFETY: r < rows and c < cols, so the slot is inside the rows * cols the array holds.
            // The write moves the new variant in without dropping the empty one it replaces.
            unsafe { base.add(r + c * rows).write(variant_of(cell)) };
        }
    }
    // SAFETY: pairs with the access above.
    let _ = unsafe { SafeArrayUnaccessData(psa) };
    Ok(out)
}

/// The cells of a variant `Range.Value2` or `Range.Formula` returned: a lone value for one cell, a
/// two-dimensional array for more.
fn read_grid(v: &VARIANT) -> XResult<Vec<Vec<ExcelCell>>> {
    // SAFETY: the array member is read only when the type tag says the variant holds an array.
    let psa: *mut SAFEARRAY = unsafe {
        let inner = &v.Anonymous.Anonymous;
        if inner.vt.0 & VT_ARRAY.0 == 0 {
            return Ok(vec![vec![cell_of(v)]]);
        }
        inner.Anonymous.parray
    };
    // SAFETY: `psa` is the live array the variant owns.
    if psa.is_null() || unsafe { SafeArrayGetDim(psa) } != 2 {
        return Err(fail(
            "Excel returned a range in a shape this tool cannot read",
        ));
    }
    // SAFETY: as above, dimensions 1 and 2 exist.
    let (r0, r1, c0, c1) = unsafe {
        (
            SafeArrayGetLBound(psa, 1).unwrap_or(1),
            SafeArrayGetUBound(psa, 1).unwrap_or(0),
            SafeArrayGetLBound(psa, 2).unwrap_or(1),
            SafeArrayGetUBound(psa, 2).unwrap_or(0),
        )
    };
    let (rows, cols) = ((r1 - r0 + 1).max(0) as usize, (c1 - c0 + 1).max(0) as usize);
    let mut data: *mut c_void = std::ptr::null_mut();
    // SAFETY: locks the live array for reading until the unaccess below.
    unsafe { SafeArrayAccessData(psa, &raw mut data) }
        .map_err(|e| fail(format!("could not read Excel's answer: {e}")))?;
    let base = data.cast::<VARIANT>();
    let mut out = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut line = Vec::with_capacity(cols);
        for c in 0..cols {
            // SAFETY: r < rows and c < cols, inside the rows * cols variants the array holds.
            line.push(cell_of(unsafe { &*base.add(r + c * rows) }));
        }
        out.push(line);
    }
    // SAFETY: pairs with the access above.
    let _ = unsafe { SafeArrayUnaccessData(psa) };
    Ok(out)
}

// ---------------------------------------------------------------------------------------- session

#[derive(Default)]
struct Session {
    app: Option<IDispatch>,
    books: HashMap<String, IDispatch>,
    made: u32,
}

impl Session {
    fn handle(&mut self, req: &ExcelRequest) -> BackendResult<ExcelReply> {
        match self.dispatch(req) {
            Ok(reply) => Ok(reply),
            Err(f) => {
                if f.gone {
                    // the person closed our Excel or it crashed: forget it, a new one can start
                    self.app = None;
                    self.books.clear();
                }
                Err(f.into())
            }
        }
    }

    fn app(&self) -> XResult<&IDispatch> {
        self.app
            .as_ref()
            .ok_or_else(|| fail("no Excel instance; excel new or excel open starts one"))
    }

    fn book(&self, handle: &str) -> XResult<&IDispatch> {
        self.books.get(handle).ok_or_else(|| {
            fail(format!(
                "no workbook {handle:?}; this tool can only use handles that excel new or excel open returned"
            ))
        })
    }

    fn dispatch(&mut self, req: &ExcelRequest) -> XResult<ExcelReply> {
        match req {
            ExcelRequest::New => {
                self.start()?;
                let book = object(
                    &method(&self.workbooks()?, "Add", Vec::new())?,
                    "Workbooks.Add",
                )?;
                self.adopt(book)
            }
            ExcelRequest::Open { path } => {
                self.start()?;
                let path = path.replace('/', "\\");
                let args = vec![VARIANT::from(path.as_str()), VARIANT::from(0i32)];
                let book = object(&method(&self.workbooks()?, "Open", args)?, "Workbooks.Open")?;
                self.adopt(book)
            }
            ExcelRequest::Write {
                workbook,
                sheet,
                range,
                values,
            } => {
                let ws = self.sheet(workbook, sheet.as_deref())?;
                let (col, row) = parse_cell(range).ok_or_else(|| fail("bad range"))?;
                let area = Area {
                    col,
                    row,
                    cols: values.first().map_or(0, Vec::len) as u32,
                    rows: values.len() as u32,
                };
                let target = range_of(&ws, &area.address())?;
                put(&target, "Formula", grid_variant(values)?)?;
                Ok(ExcelReply::default())
            }
            ExcelRequest::Read {
                workbook,
                sheet,
                range,
                formulas,
            } => {
                let ws = self.sheet(workbook, sheet.as_deref())?;
                let area = parse_area(range).ok_or_else(|| fail("bad range"))?;
                let target = range_of(&ws, &area.address())?;
                Ok(ExcelReply {
                    values: read_grid(&get(&target, "Value2")?)?,
                    formulas: if *formulas {
                        read_grid(&get(&target, "Formula")?)?
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                })
            }
            ExcelRequest::AddSheet { workbook, name } => {
                let book = self.book(workbook)?;
                let sheets = get_object(book, "Worksheets")?;
                let count = number_of(&get(&sheets, "Count")?).unwrap_or(1.0) as i32;
                let last = object(
                    &method(&sheets, "Item", vec![VARIANT::from(count)])?,
                    "Worksheets.Item",
                )?;
                // Add(Before, After): after the last sheet
                let added = object(
                    &method(&sheets, "Add", vec![missing(), VARIANT::from(last)])?,
                    "Worksheets.Add",
                )?;
                if let Err(e) = put(&added, "Name", VARIANT::from(name.as_str())) {
                    // a bad or taken name leaves a stray sheet behind; take it away again
                    let _ = method(&added, "Delete", Vec::new());
                    return Err(e);
                }
                Ok(ExcelReply {
                    sheets: sheet_names(book)?,
                    ..Default::default()
                })
            }
            ExcelRequest::Calc => {
                method(self.app()?, "Calculate", Vec::new())?;
                Ok(ExcelReply::default())
            }
            ExcelRequest::SaveAs { workbook, path } => {
                let book = self.book(workbook)?;
                let path = path.replace('/', "\\");
                let args = vec![
                    VARIANT::from(path.as_str()),
                    VARIANT::from(XL_OPEN_XML_WORKBOOK),
                ];
                method(book, "SaveAs", args)?;
                Ok(ExcelReply::default())
            }
            ExcelRequest::Close { workbook } => {
                let book = self.book(workbook)?;
                // Close(SaveChanges := False)
                method(book, "Close", vec![VARIANT::from(false)])?;
                self.books.remove(workbook);
                let last = self.books.is_empty();
                if last {
                    self.quit();
                }
                Ok(ExcelReply {
                    instance_quit: last,
                    ..Default::default()
                })
            }
        }
    }

    fn workbooks(&self) -> XResult<IDispatch> {
        get_object(self.app()?, "Workbooks")
    }

    fn adopt(&mut self, book: IDispatch) -> XResult<ExcelReply> {
        self.made += 1;
        let handle = format!("x{}", self.made);
        let sheets = sheet_names(&book)?;
        self.books.insert(handle.clone(), book);
        Ok(ExcelReply {
            workbook: Some(handle),
            sheets,
            ..Default::default()
        })
    }

    fn sheet(&self, workbook: &str, name: Option<&str>) -> XResult<IDispatch> {
        let sheets = get_object(self.book(workbook)?, "Worksheets")?;
        let which = match name {
            Some(n) => VARIANT::from(n),
            None => VARIANT::from(1i32),
        };
        method(&sheets, "Item", vec![which])
            .and_then(|v| object(&v, "Worksheets.Item"))
            .map_err(|e| match name {
                Some(n) if !e.gone => fail(format!("no sheet named {n:?} in {workbook}")),
                _ => e,
            })
    }

    /// Start our own Excel, or keep the one we have.
    fn start(&mut self) -> XResult<()> {
        if self.app.is_some() {
            return Ok(());
        }
        let before = excel_pids();
        // SAFETY: a registry lookup of a constant, NUL-terminated name.
        let clsid = unsafe { CLSIDFromProgID(w!("Excel.Application")) }
            .map_err(|_| fail("Excel is not installed"))?;
        // SAFETY: creates the Excel automation object in its own process.
        let app: IDispatch = unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|e| fail(format!("could not start Excel: {e}")))?;
        // Never touch the person's Excel. A fresh process has a pid that was not running a moment ago.
        if !before.is_empty() {
            let hwnd = number_of(&get(&app, "Hwnd")?).unwrap_or(0.0) as isize;
            let mut pid = 0u32;
            // SAFETY: only reads the owning pid of a window handle into a local.
            unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut c_void), Some(&raw mut pid)) };
            if pid == 0 || before.contains(&pid) {
                // dropping `app` only releases our reference; it does not quit anything
                return Err(fail(
                    "Windows handed back an Excel that was already running; this tool will not use it",
                ));
            }
        }
        put(&app, "Visible", VARIANT::from(false))?;
        put(&app, "DisplayAlerts", VARIANT::from(false))?;
        // best effort: no macros, no workbook events, no link prompts
        let _ = put(
            &app,
            "AutomationSecurity",
            VARIANT::from(FORCE_DISABLE_MACROS),
        );
        let _ = put(&app, "EnableEvents", VARIANT::from(false));
        let _ = put(&app, "AskToUpdateLinks", VARIANT::from(false));
        self.app = Some(app);
        Ok(())
    }

    /// Close what is open without saving and quit our Excel. Best effort, safe to repeat.
    fn quit(&mut self) {
        for (_, book) in self.books.drain() {
            let _ = method(&book, "Close", vec![VARIANT::from(false)]);
        }
        if let Some(app) = self.app.take() {
            let _ = method(&app, "Quit", Vec::new());
        }
    }
}

fn range_of(sheet: &IDispatch, address: &str) -> XResult<IDispatch> {
    object(
        &method(sheet, "Range", vec![VARIANT::from(address)])?,
        "Worksheet.Range",
    )
}

fn sheet_names(book: &IDispatch) -> XResult<Vec<String>> {
    let sheets = get_object(book, "Worksheets")?;
    let count = number_of(&get(&sheets, "Count")?).unwrap_or(0.0) as i32;
    let mut names = Vec::new();
    for i in 1..=count {
        let sheet = object(
            &method(&sheets, "Item", vec![VARIANT::from(i)])?,
            "Worksheets.Item",
        )?;
        names.push(text_of(&get(&sheet, "Name")?).unwrap_or_default());
    }
    Ok(names)
}

/// Process ids of every Excel running right now.
fn excel_pids() -> Vec<u32> {
    crate::process::process_table()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.exe_name.eq_ignore_ascii_case("excel.exe"))
        .map(|p| p.pid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// One live Excel at a time: two tests starting one each would see the other's as "the
    /// person's" and rightly refuse.
    static LIVE: Mutex<()> = Mutex::new(());

    #[test]
    fn excel_error_values_read_as_the_cells_show_them() {
        assert_eq!(error_text(0x800A_07D7_u32 as i32), "#DIV/0!");
        assert_eq!(error_text(0x800A_07FA_u32 as i32), "#N/A");
        assert_eq!(error_text(0x800A_07ED_u32 as i32), "#NAME?");
        assert_eq!(error_text(5), "#ERROR!");
    }

    #[test]
    fn excel_without_an_instance_refuses_everything_but_new_and_open() {
        let host = Host::default();
        let err = host.run(&ExcelRequest::Calc).unwrap_err();
        assert!(err.to_string().contains("no Excel instance"), "{err}");
        host.shutdown();
    }

    fn cells(rows: &[&[ExcelCell]]) -> Vec<Vec<ExcelCell>> {
        rows.iter().map(|r| r.to_vec()).collect()
    }

    #[test]
    #[ignore = "drives a real hidden Excel; run with: cargo test -p wh-win --release -- --ignored excel"]
    fn excel_hidden_instance_round_trip() {
        let _one = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        let started = Instant::now();
        let host = Host::default();
        let wb = host
            .run(&ExcelRequest::New)
            .expect("new")
            .workbook
            .expect("handle");
        eprintln!("new: {:?}", started.elapsed());
        let t = Instant::now();
        let t_cell = |s: &str| ExcelCell::Text(s.into());
        let n = ExcelCell::Number;
        host.run(&ExcelRequest::Write {
            workbook: wb.clone(),
            sheet: None,
            range: "A1".into(),
            values: cells(&[
                &[n(1.0), n(2.0), n(3.0)],
                &[n(4.0), n(5.0), n(6.0)],
                &[
                    t_cell("hello"),
                    ExcelCell::Bool(true),
                    t_cell("=SUM(A1:C2)"),
                ],
            ]),
        })
        .expect("write");
        let read = host
            .run(&ExcelRequest::Read {
                workbook: wb.clone(),
                sheet: None,
                range: "A1:C3".into(),
                formulas: true,
            })
            .expect("read");
        eprintln!("3x3 write + read: {:?}", t.elapsed());
        assert_eq!(read.values[0], vec![n(1.0), n(2.0), n(3.0)]);
        assert_eq!(read.values[2][0], t_cell("hello"));
        assert_eq!(read.values[2][1], ExcelCell::Bool(true));
        assert_eq!(read.values[2][2], n(21.0), "the SUM formula was worked out");
        assert_eq!(read.formulas[2][2], t_cell("=SUM(A1:C2)"));
        // one cell reads as a 1x1 grid; a second sheet; a recalculation
        let one = host
            .run(&ExcelRequest::Read {
                workbook: wb.clone(),
                sheet: None,
                range: "B2".into(),
                formulas: false,
            })
            .expect("one cell");
        assert_eq!(one.values, vec![vec![n(5.0)]]);
        let added = host
            .run(&ExcelRequest::AddSheet {
                workbook: wb.clone(),
                name: "Data".into(),
            })
            .expect("add sheet");
        assert_eq!(added.sheets, vec!["Sheet1".to_string(), "Data".to_string()]);
        assert!(
            host.run(&ExcelRequest::AddSheet {
                workbook: wb.clone(),
                name: "Data".into(),
            })
            .is_err(),
            "a taken name is refused"
        );
        host.run(&ExcelRequest::Calc).expect("calc");
        // save to %TEMP%, check the file, close
        let path = std::env::temp_dir().join(format!("hekaton-excel-{}.xlsx", std::process::id()));
        host.run(&ExcelRequest::SaveAs {
            workbook: wb.clone(),
            path: path.to_string_lossy().into_owned(),
        })
        .expect("save_as");
        assert!(std::fs::metadata(&path).expect("saved file").len() > 1000);
        let closed = host
            .run(&ExcelRequest::Close {
                workbook: wb.clone(),
            })
            .expect("close");
        assert!(closed.instance_quit);
        assert!(
            host.run(&ExcelRequest::Close { workbook: wb }).is_err(),
            "a closed handle is gone"
        );
        // the saved file opens again in a fresh owned instance
        let reopened = host
            .run(&ExcelRequest::Open {
                path: path.to_string_lossy().into_owned(),
            })
            .expect("open");
        let again = host
            .run(&ExcelRequest::Read {
                workbook: reopened.workbook.clone().expect("handle"),
                sheet: Some("Sheet1".into()),
                range: "C3".into(),
                formulas: false,
            })
            .expect("read reopened");
        assert_eq!(again.values, vec![vec![n(21.0)]]);
        host.shutdown();
        let _ = std::fs::remove_file(&path);
        eprintln!("whole round trip: {:?}", started.elapsed());
        // the instance really went away
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            excel_pids().is_empty()
        });
        assert!(gone, "an Excel process is still running after shutdown");
    }

    #[test]
    #[ignore = "drives a real hidden Excel; run with: cargo test -p wh-win --release -- --ignored excel"]
    fn excel_a_second_owner_gets_its_own_process_and_leaves_the_first_alone() {
        let _one = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        let (a, b) = (Host::default(), Host::default());
        a.run(&ExcelRequest::New).expect("first instance");
        // the first is "somebody else's Excel" to the second, which must still get its own
        let wb = b
            .run(&ExcelRequest::New)
            .expect("second instance")
            .workbook
            .expect("handle");
        assert_eq!(excel_pids().len(), 2, "two processes, not one shared");
        b.run(&ExcelRequest::Close { workbook: wb }).expect("close");
        // Excel takes a moment to exit; the first must be the one left
        let left_one = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            excel_pids().len() == 1
        });
        assert!(left_one, "closing the second left the first running");
        a.shutdown();
        b.shutdown();
    }

    #[test]
    #[ignore = "drives a real hidden Excel; run with: cargo test -p wh-win --release -- --ignored excel"]
    fn excel_writes_a_hundred_thousand_rows_in_one_call() {
        let _one = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        let host = Host::default();
        let wb = host
            .run(&ExcelRequest::New)
            .expect("new")
            .workbook
            .expect("handle");
        let rows = 100_000usize;
        let values: Vec<Vec<ExcelCell>> = (0..rows)
            .map(|r| {
                vec![
                    ExcelCell::Number(r as f64),
                    ExcelCell::Text(format!("row {r}")),
                    ExcelCell::Number(r as f64 * 1.5),
                    ExcelCell::Bool(r % 2 == 0),
                    ExcelCell::Text(format!("=A{}*2", r + 1)),
                ]
            })
            .collect();
        let t = Instant::now();
        host.run(&ExcelRequest::Write {
            workbook: wb.clone(),
            sheet: None,
            range: "A1".into(),
            values,
        })
        .expect("write 100k x 5");
        eprintln!("100000 x 5 write: {:?}", t.elapsed());
        let t = Instant::now();
        let last = host
            .run(&ExcelRequest::Read {
                workbook: wb,
                sheet: None,
                range: format!("A{rows}:E{rows}"),
                formulas: false,
            })
            .expect("read last row");
        eprintln!("read of the last row: {:?}", t.elapsed());
        assert_eq!(
            last.values[0][1],
            ExcelCell::Text(format!("row {}", rows - 1))
        );
        assert_eq!(
            last.values[0][4],
            ExcelCell::Number((rows - 1) as f64 * 2.0)
        );
        host.shutdown();
    }
}
