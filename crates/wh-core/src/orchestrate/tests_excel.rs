//! The `excel` power channel over the fake workbook: the gates, the checks that run before Excel is
//! asked, a write and read round trip, and what the audit log keeps. No Excel is involved.

use serde_json::{Value, json};

use super::testkit::Kit;
use crate::backend::ExcelRequest;
use crate::env::MapEnv;
use crate::testutil::TempDir;

fn on() -> MapEnv {
    MapEnv::new()
        .with("WINHANDS_POWER_CHANNELS", "excel")
        .with("WINHANDS_AUTO_SHOT", "0")
}

fn kit() -> Kit {
    Kit::with_env(on())
}

/// One excel call through the whole daemon route; the error text when it was refused.
fn call(kit: &Kit, v: Value) -> Result<String, String> {
    let out = kit.computer.execute(&v, &kit.who);
    if out.ok {
        Ok(out.result.unwrap_or_default())
    } else {
        Err(out.error.unwrap_or_default())
    }
}

fn new_book(kit: &Kit) -> String {
    let note = call(kit, json!({"action": "excel", "op": "new"})).unwrap();
    assert!(note.contains("x1"), "{note}");
    "x1".into()
}

#[test]
fn it_is_off_until_the_owner_turns_it_on() {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_AUTO_SHOT", "0"));
    let why = call(&kit, json!({"action": "excel", "op": "new"})).unwrap_err();
    assert!(
        why.contains("power channel") && why.contains("off"),
        "{why}"
    );
    assert!(kit.fake.excel_requests().is_empty());
    assert!(!kit.fake.excel_running());
    // a channel that is named but is not excel stays off too
    let other = Kit::with_env(MapEnv::new().with("WINHANDS_POWER_CHANNELS", "word, pdf"));
    assert!(call(&other, json!({"action": "excel", "op": "new"})).is_err());
    // on, with spaces and capitals as an owner may type them
    let loud = Kit::with_env(MapEnv::new().with("WINHANDS_POWER_CHANNELS", " Word , EXCEL "));
    assert!(call(&loud, json!({"action": "excel", "op": "new"})).is_ok());
}

#[test]
fn an_allowlist_without_excel_refuses_it() {
    let kit = kit();
    let why = call(
        &kit,
        json!({"action": "excel", "op": "new", "only_apps": ["notepad.exe"]}),
    )
    .unwrap_err();
    assert!(why.contains("excel.exe"), "{why}");
    assert!(kit.fake.excel_requests().is_empty());
    let ok = call(
        &kit,
        json!({"action": "excel", "op": "new", "only_apps": ["notepad.exe", "excel.exe"]}),
    );
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn read_only_allows_a_read_and_nothing_else() {
    let kit = kit();
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "range": "A1", "values": [[1]]}),
    )
    .unwrap();
    let before = kit.fake.excel_requests().len();
    for step in [
        json!({"action": "excel", "op": "new"}),
        json!({"action": "excel", "op": "write", "workbook": "x1", "values": [[2]]}),
        json!({"action": "excel", "op": "close", "workbook": "x1"}),
        json!({"action": "excel", "op": "calc"}),
    ] {
        let mut single = step.clone();
        single["read_only"] = json!(true);
        let why = call(&kit, single).unwrap_err();
        assert!(why.contains("read_only"), "{why}");
        // the same step in a batch is refused before any step runs
        let why = call(&kit, json!({"read_only": true, "actions": [step]})).unwrap_err();
        assert!(why.contains("read_only"), "{why}");
    }
    assert_eq!(kit.fake.excel_requests().len(), before);
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": "x1", "range": "A1", "read_only": true}),
    )
    .unwrap();
    assert!(read.contains("[[1]]"), "{read}");
    // a read in a batch passes the read_only preflight too
    call(
        &kit,
        json!({"read_only": true, "actions": [
            {"action": "excel", "op": "read", "workbook": "x1", "range": "A1"}
        ]}),
    )
    .unwrap();
}

#[test]
fn the_gaming_lock_stops_a_write_but_not_a_read() {
    let kit = kit();
    let wb = new_book(&kit);
    let dir = kit.computer.config().paths().dir().to_path_buf();
    crate::safety::lock(&dir, "playing").unwrap();
    let why = call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [[1]]}),
    )
    .unwrap_err();
    assert!(why.contains("gaming lock"), "{why}");
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": "x1", "range": "A1"}),
    );
    assert!(read.is_ok(), "{read:?}");
    crate::safety::unlock(&dir).unwrap();
    assert!(
        call(
            &kit,
            json!({"action": "excel", "op": "write", "workbook": "x1", "values": [[1]]})
        )
        .is_ok()
    );
}

#[test]
fn a_write_and_a_read_round_trip_with_a_formula() {
    let kit = kit();
    let wb = new_book(&kit);
    let wrote = call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "range": "B2", "values": [
            [1, 2.5, true],
            ["text", null, "=SUM(B2:C2)"]
        ]}),
    )
    .unwrap();
    assert!(wrote.contains("2 rows x 3 cols (6 cells) at B2"), "{wrote}");
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "B2:D3", "formulas": true}),
    )
    .unwrap();
    assert!(
        read.contains(r#""values":[[1,2.5,true],["text",null,3.5]]"#),
        "{read}"
    );
    assert!(
        read.contains(r#""formulas":[[1,2.5,true],["text",null,"=SUM(B2:C2)"]]"#),
        "{read}"
    );
    // without formulas:true the formulas stay out
    let plain = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "B2"}),
    )
    .unwrap();
    assert!(
        plain.contains("[[1]]") && !plain.contains("formulas"),
        "{plain}"
    );
    // a second sheet
    let sheet = call(
        &kit,
        json!({"action": "excel", "op": "add_sheet", "workbook": wb, "name": "Data"}),
    )
    .unwrap();
    assert!(sheet.contains("Sheet1, Data"), "{sheet}");
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "sheet": "Data", "values": [["on data"]]}),
    )
    .unwrap();
    let data = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "sheet": "Data", "range": "A1"}),
    )
    .unwrap();
    assert!(data.contains(r#"[["on data"]]"#), "{data}");
    assert!(
        call(
            &kit,
            json!({"action": "excel", "op": "read", "workbook": wb, "sheet": "Nope", "range": "A1"})
        )
        .is_err()
    );
    assert_eq!(
        call(&kit, json!({"action": "excel", "op": "calc"})).unwrap(),
        "excel: recalculated"
    );
}

#[test]
fn bad_values_and_ranges_are_refused_before_excel_hears_of_them() {
    let kit = kit();
    let wb = new_book(&kit);
    let before = kit.fake.excel_requests().len();
    for args in [
        json!({"values": [[1, 2], [3]]}),
        json!({"values": [[{"a": 1}]]}),
        json!({"values": [[1], 2]}),
        json!({"values": []}),
        json!({"values": [[]]}),
        json!({"values": "x"}),
        json!({}),
        json!({"range": "A1:B2", "values": [[1]]}),
        json!({"range": "XFD1", "values": [[1, 2]]}),
        json!({"range": "A1048576", "values": [[1], [2]]}),
        json!({"range": "Sheet1!A1", "values": [[1]]}),
    ] {
        let mut step = json!({"action": "excel", "op": "write", "workbook": wb});
        step.as_object_mut()
            .unwrap()
            .extend(args.as_object().unwrap().clone());
        assert!(call(&kit, step.clone()).is_err(), "{step}");
    }
    for step in [
        json!({"action": "excel", "op": "read", "workbook": wb}),
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "A:A"}),
        json!({"action": "excel", "op": "write", "values": [[1]]}),
        json!({"action": "excel", "op": "add_sheet", "workbook": wb}),
        json!({"action": "excel", "op": "launch"}),
        json!({"action": "excel"}),
    ] {
        assert!(call(&kit, step.clone()).is_err(), "{step}");
    }
    assert_eq!(kit.fake.excel_requests().len(), before);
}

#[test]
fn a_write_over_the_cell_cap_is_refused() {
    let kit = kit();
    let wb = new_book(&kit);
    let row = vec![Value::Null; 1_000_001];
    let why = call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [row.clone(), row]}),
    )
    .unwrap_err();
    assert!(
        why.contains("2000002 cells") && why.contains("cap"),
        "{why}"
    );
    assert_eq!(
        kit.fake.excel_requests().len(),
        1,
        "only the new reached Excel"
    );
}

#[test]
fn a_read_over_the_configured_cap_is_cut_and_says_so() {
    let kit = Kit::with_env(on().with("WINHANDS_EXCEL_READ_MAX_CELLS", "6"));
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [
            [1, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12]
        ]}),
    )
    .unwrap();
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "A1:C4"}),
    )
    .unwrap();
    assert!(
        read.contains("TRUNCATED to the first 2 of 4 rows"),
        "{read}"
    );
    assert!(
        read.contains("[[1,2,3],[4,5,6]]") && !read.contains("7"),
        "{read}"
    );
    let asked = kit
        .fake
        .excel_requests()
        .into_iter()
        .find_map(|r| match r {
            ExcelRequest::Read { range, .. } => Some(range),
            _ => None,
        })
        .unwrap();
    assert_eq!(asked, "A1:C2", "Excel was only asked for the rows kept");
    let wide = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "A1:H1"}),
    )
    .unwrap_err();
    assert!(wide.contains("columns wide"), "{wide}");
}

#[test]
fn save_as_keeps_the_destructive_path_rule_and_will_not_overwrite() {
    let kit = kit();
    let wb = new_book(&kit);
    let save = |path: &str, extra: Value| {
        let mut step = json!({"action": "excel", "op": "save_as", "workbook": wb, "path": path});
        step.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        call(&kit, step)
    };
    let why = save(r"C:\Windows\System32\out.xlsx", json!({})).unwrap_err();
    assert!(why.contains("system"), "{why}");
    let why = save(r"C:\Windows\out.xlsx", json!({"overwrite": true})).unwrap_err();
    assert!(why.contains("system"), "{why}");
    let why = save("relative.xlsx", json!({})).unwrap_err();
    assert!(why.contains("full path"), "{why}");
    let dir = kit.tmp.path();
    let csv = dir.join("a.csv").to_string_lossy().into_owned();
    assert!(save(&csv, json!({})).unwrap_err().contains(".xlsx"));
    assert_eq!(kit.fake.excel_saved_as("x1"), None);

    let target = dir.join("out.xlsx");
    let target_s = target.to_string_lossy().into_owned();
    let done = save(&target_s, json!({})).unwrap();
    assert!(done.contains("saved x1 as"), "{done}");
    assert_eq!(kit.fake.excel_saved_as("x1"), Some(target_s.clone()));
    // the fake writes nothing, so make the file exist the way Excel would have
    std::fs::write(&target, b"x").unwrap();
    let why = save(&target_s, json!({})).unwrap_err();
    assert!(why.contains("overwrite"), "{why}");
    assert!(save(&target_s, json!({"overwrite": true})).is_ok());
    // %TEMP% is expanded before the path is judged
    let via_var = save("%TEMP%\\viavar.xlsx", json!({})).unwrap();
    assert!(
        via_var.contains("viavar.xlsx") && !via_var.contains('%'),
        "{via_var}"
    );
}

#[test]
fn a_handle_this_tool_did_not_create_cannot_be_closed() {
    let kit = kit();
    let wb = new_book(&kit);
    let why = call(
        &kit,
        json!({"action": "excel", "op": "close", "workbook": "x9"}),
    )
    .unwrap_err();
    assert!(why.contains("x9"), "{why}");
    assert!(
        kit.fake.excel_running(),
        "the owned instance was left alone"
    );
    assert!(call(&kit, json!({"action": "excel", "op": "close"})).is_err());
    let closed = call(
        &kit,
        json!({"action": "excel", "op": "close", "workbook": wb}),
    )
    .unwrap();
    assert!(
        closed.contains("closed x1") && closed.contains("quit"),
        "{closed}"
    );
    assert!(!kit.fake.excel_running());
    assert_eq!(kit.fake.excel_quits(), 1);
    // a closed handle is gone
    assert!(
        call(
            &kit,
            json!({"action": "excel", "op": "close", "workbook": "x1"})
        )
        .is_err()
    );
}

#[test]
fn the_instance_quits_with_two_books_only_after_the_last() {
    let kit = kit();
    new_book(&kit);
    call(&kit, json!({"action": "excel", "op": "new"})).unwrap();
    let first = call(
        &kit,
        json!({"action": "excel", "op": "close", "workbook": "x1"}),
    )
    .unwrap();
    assert!(!first.contains("quit"), "{first}");
    assert!(kit.fake.excel_running());
    call(
        &kit,
        json!({"action": "excel", "op": "close", "workbook": "x2"}),
    )
    .unwrap();
    assert!(!kit.fake.excel_running());
}

#[test]
fn dropping_the_tool_quits_the_owned_instance() {
    let kit = kit();
    new_book(&kit);
    let fake = kit.fake.clone();
    drop(kit.computer);
    assert!(!fake.excel_running());
    assert_eq!(fake.excel_quits(), 1);
}

#[test]
fn status_says_whether_excel_can_be_driven() {
    let kit = kit();
    assert_eq!(kit.computer.status()["excel"], json!(true));
    assert_eq!(kit.computer.status()["power_channels"], json!(["excel"]));
    kit.fake.set_excel_available(false);
    assert_eq!(kit.computer.status()["excel"], json!(false));
    let why = call(&kit, json!({"action": "excel", "op": "new"})).unwrap_err();
    assert!(why.contains("Excel"), "{why}");
}

#[test]
fn an_excel_step_in_a_batch_sends_no_input_and_takes_no_picture() {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_POWER_CHANNELS", "excel"));
    let done = kit
        .run(json!({"actions": [
            {"action": "excel", "op": "new"},
            {"action": "excel", "op": "write", "workbook": "x1", "values": [[1, 2], [3, 4]]},
            {"action": "excel", "op": "read", "workbook": "x1", "range": "A1:B2"}
        ]}))
        .unwrap();
    assert!(done.failure.is_none(), "{:?}", done.failure);
    assert_eq!(done.completed_steps, 3);
    assert!(done.images.is_empty());
    assert!(!done.input_sent);
    assert!(done.text.contains("[[1,2],[3,4]]"), "{}", done.text);
    assert!(!done.text.contains("screen settled"), "{}", done.text);
    assert!(kit.fake.events().is_empty());
}

#[test]
fn strict_opens_only_under_temp_or_what_it_saved() {
    let strict = Kit::with_env(on().with("WINHANDS_SAFETY_PROFILE", "strict"));
    let elsewhere = TempDir::new("excel-elsewhere");
    let outside = elsewhere.path().join("book.xlsx");
    std::fs::write(&outside, b"x").unwrap();
    let inside = strict.tmp.path().join("book.xlsx");
    std::fs::write(&inside, b"x").unwrap();
    let open = |kit: &Kit, path: &std::path::Path| {
        call(
            kit,
            json!({"action": "excel", "op": "open", "path": path.to_string_lossy(),
                   "only_apps": ["excel.exe"]}),
        )
    };
    let why = open(&strict, &outside).unwrap_err();
    assert!(why.contains("strict") && why.contains("%TEMP%"), "{why}");
    assert!(open(&strict, &inside).is_ok());
    // a save made in this very call may be opened again, wherever it went
    let dest = elsewhere.path().join("saved.xlsx");
    let saved = strict
        .run(json!({"only_apps": ["excel.exe"], "actions": [
            {"action": "excel", "op": "new"},
            {"action": "excel", "op": "save_as", "workbook": "x2", "path": dest.to_string_lossy()},
        ]}))
        .unwrap();
    assert!(saved.failure.is_none(), "{:?}", saved.failure);
    let dest2 = elsewhere.path().join("saved-2.xlsx");
    let again = strict
        .run(json!({"only_apps": ["excel.exe"], "actions": [
            {"action": "excel", "op": "save_as", "workbook": "x2", "path": dest2.to_string_lossy()},
            {"action": "excel", "op": "open", "path": dest2.to_string_lossy()},
        ]}))
        .unwrap();
    assert!(again.failure.is_none(), "{:?}", again.failure);
    // the model's overwrite flag is not the owner's approval under strict
    let why = call(
        &strict,
        json!({"action": "excel", "op": "save_as", "workbook": "x2",
               "path": dest.to_string_lossy(), "overwrite": true, "only_apps": ["excel.exe"]}),
    )
    .unwrap_err();
    assert!(why.contains("strict") && why.contains("replace"), "{why}");
    // the normal profile does not care where the file is
    let normal = kit();
    assert!(open(&normal, &outside).is_ok());
    // system paths are refused in every profile
    let why = call(
        &normal,
        json!({"action": "excel", "op": "open", "path": r"C:\Windows\System32\config\SAM"}),
    )
    .unwrap_err();
    assert!(why.contains("system"), "{why}");
}

#[test]
fn the_audit_log_records_where_and_how_much_but_never_a_cell() {
    let kit = kit();
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "sheet": "Sheet1", "range": "A1",
               "values": [["SECRET-ALPHA", 2], ["SECRET-BETA", "=A2"]]}),
    )
    .unwrap();
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "A1:B2"}),
    )
    .unwrap();
    assert!(
        read.contains("SECRET-ALPHA"),
        "the caller does get the cells"
    );
    // a refused write logs through the failure route, which logs the arguments
    let _ = call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": "x77", "values": [["SECRET-GAMMA"]]}),
    );
    // and so does a batch
    kit.clock.advance(std::time::Duration::from_secs(2));
    call(
        &kit,
        json!({"actions": [
            {"action": "excel", "op": "write", "workbook": wb, "range": "D1", "values": [["SECRET-DELTA"]]},
            {"action": "excel", "op": "read", "workbook": wb, "range": "D1"}
        ]}),
    )
    .unwrap();
    let log = std::fs::read_to_string(kit.computer.config().paths().audit_log()).unwrap();
    assert!(!log.contains("SECRET"), "{log}");
    assert!(
        log.contains(r#""op": "write""#) && log.contains(r#""sheet": "Sheet1""#),
        "{log}"
    );
    assert!(
        log.contains(r#""range": "A1""#) && log.contains(r#""rows": 2"#),
        "{log}"
    );
    assert!(log.contains(r#""cols": 2"#), "{log}");
    assert!(log.contains("excel cells are not logged"), "{log}");
}
