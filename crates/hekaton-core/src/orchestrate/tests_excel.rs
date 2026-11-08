//! The `excel` power channel over the fake workbook: the gates, the checks that run before Excel is
//! asked, a write and read round trip, and what the audit log keeps. No Excel is involved.

use serde_json::{Value, json};

use super::testkit::Kit;
use crate::backend::ExcelRequest;
use crate::env::MapEnv;
use crate::testutil::TempDir;

fn on() -> MapEnv {
    MapEnv::new()
        .with("HEKATON_POWER_CHANNELS", "excel")
        .with("HEKATON_AUTO_SHOT", "0")
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
    let kit = Kit::with_env(MapEnv::new().with("HEKATON_AUTO_SHOT", "0"));
    let why = call(&kit, json!({"action": "excel", "op": "new"})).unwrap_err();
    assert!(
        why.contains("power channel") && why.contains("off"),
        "{why}"
    );
    assert!(kit.fake.excel_requests().is_empty());
    assert!(!kit.fake.excel_running());
    // a channel that is named but is not excel stays off too
    let other = Kit::with_env(MapEnv::new().with("HEKATON_POWER_CHANNELS", "word, pdf"));
    assert!(call(&other, json!({"action": "excel", "op": "new"})).is_err());
    // on, with spaces and capitals as an owner may type them
    let loud = Kit::with_env(MapEnv::new().with("HEKATON_POWER_CHANNELS", " Word , EXCEL "));
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
    let kit = Kit::with_env(on().with("HEKATON_EXCEL_READ_MAX_CELLS", "6"));
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
    let kit = Kit::with_env(MapEnv::new().with("HEKATON_POWER_CHANNELS", "excel"));
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
    let strict = Kit::with_env(on().with("HEKATON_SAFETY_PROFILE", "strict"));
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

// ------------------------------------------------------------- format_conditional, chart, pivot

use crate::backend::{ExcelChartKind, ExcelRule};

/// A small sales table in A1:D7 of a new workbook: Region, Product, Qty, Price.
fn sales_book(kit: &Kit) -> String {
    let wb = new_book(kit);
    call(
        kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "range": "A1", "values": [
            ["Region", "Product", "Qty", "Price"],
            ["East", "Nut", 10, 1.5],
            ["West", "Nut", 5, 1.5],
            ["East", "Bolt", 7, 2],
            ["West", "Bolt", 3, 2],
            ["East", "Nut", 1, 1.5],
            ["North", "Bolt", 4, 2]
        ]}),
    )
    .unwrap();
    wb
}

fn step(base: Value, extra: Value) -> Value {
    let mut out = base;
    out.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    out
}

#[test]
fn a_conditional_format_reaches_excel_with_its_rule_and_a_converted_color() {
    let kit = kit();
    let wb = sales_book(&kit);
    let fc = |extra: Value| {
        call(
            &kit,
            step(
                json!({"action": "excel", "op": "format_conditional", "workbook": wb,
                       "sheet": "Sheet1", "range": "C2:C7"}),
                extra,
            ),
        )
    };
    let said = fc(json!({"rule": "greater_than", "value": 5, "color": "#FF0000"})).unwrap();
    assert!(
        said.contains("greater_than") && said.contains("Sheet1!C2:C7"),
        "{said}"
    );
    fc(json!({"rule": "LESS_THAN", "value": 0, "color": "#00ff00"})).unwrap();
    fc(json!({"rule": "between", "value": "2", "value2": 6.5, "color": "#0000FF"})).unwrap();
    fc(json!({"rule": "color_scale"})).unwrap();
    let rules: Vec<ExcelRule> = kit
        .fake
        .excel_requests()
        .into_iter()
        .filter_map(|r| match r {
            ExcelRequest::FormatConditional { rule, range, .. } => {
                assert_eq!(range, "C2:C7");
                Some(rule)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        rules,
        vec![
            ExcelRule::GreaterThan {
                value: 5.0,
                color: 0x0000FF
            },
            ExcelRule::LessThan {
                value: 0.0,
                color: 0x00FF00
            },
            ExcelRule::Between {
                low: 2.0,
                high: 6.5,
                color: 0xFF0000
            },
            ExcelRule::ColorScale,
        ]
    );
}

#[test]
fn a_chart_names_itself_and_says_where_it_went() {
    let kit = kit();
    let wb = sales_book(&kit);
    let first = call(
        &kit,
        json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:D7", "kind": "column",
               "title": "Sales"}),
    )
    .unwrap();
    assert!(
        first.contains("\"Chart 1\"") && first.contains("column") && first.contains("at G1"),
        "{first}"
    );
    let second = call(
        &kit,
        json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:D7", "kind": "Pie",
               "anchor": "$K$3"}),
    )
    .unwrap();
    assert!(
        second.contains("\"Chart 2\"") && second.contains("at K3"),
        "{second}"
    );
    // a text `at` is a cell too, for callers that skip the MCP shim
    let third = call(
        &kit,
        json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:D7", "kind": "line",
               "at": "B20"}),
    )
    .unwrap();
    assert!(third.contains("at B20"), "{third}");
    let charts: Vec<_> = kit
        .fake
        .excel_requests()
        .into_iter()
        .filter_map(|r| match r {
            ExcelRequest::Chart {
                kind, title, at, ..
            } => Some((kind, title, at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        charts,
        vec![
            (ExcelChartKind::Column, Some("Sales".to_string()), None),
            (ExcelChartKind::Pie, None, Some("K3".to_string())),
            (ExcelChartKind::Line, None, Some("B20".to_string())),
        ]
    );
    for (kind, xl) in [
        (ExcelChartKind::Column, 51),
        (ExcelChartKind::Line, 4),
        (ExcelChartKind::Bar, 57),
        (ExcelChartKind::Pie, 5),
    ] {
        assert_eq!(kind.xl_type(), xl);
    }
}

#[test]
fn a_pivot_sums_by_row_field_on_a_new_sheet() {
    let kit = kit();
    let wb = sales_book(&kit);
    let said = call(
        &kit,
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:D7",
               "rows": "Region", "sum_field": "Qty"}),
    )
    .unwrap();
    assert!(
        said.contains("\"Pivot\"") && said.contains("table at A3:B7"),
        "{said}"
    );
    let table = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "sheet": "Pivot", "range": "A3:B7"}),
    )
    .unwrap();
    assert!(
        table.contains(
            r#"[["Row Labels","Sum of Qty"],["East",18],["North",4],["West",8],["Grand Total",30]]"#
        ),
        "{table}"
    );
    // headers match without regard to case, a column field adds a grand total column, and the new
    // sheet takes the name asked for
    let cross = call(
        &kit,
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:D7", "rows": "region",
               "columns": "Product", "sum_field": " qty ", "name": "ByProduct"}),
    )
    .unwrap();
    assert!(
        cross.contains("\"ByProduct\"") && cross.contains("A3:D8"),
        "{cross}"
    );
    let grid = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "sheet": "ByProduct", "range": "A5:D8"}),
    )
    .unwrap();
    assert!(
        grid.contains(r#"["East",7,11,18]"#) && grid.contains(r#"["Grand Total",14,16,30]"#),
        "{grid}"
    );
    // the same name twice is Excel's refusal, passed on
    let why = call(
        &kit,
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:D7",
               "rows": "Region", "sum_field": "Qty"}),
    )
    .unwrap_err();
    assert!(why.contains("Pivot") && why.contains("exists"), "{why}");
}

#[test]
fn the_new_ops_are_acting_ops_and_follow_the_write_gates() {
    let kit = kit();
    let wb = sales_book(&kit);
    let steps = [
        json!({"action": "excel", "op": "format_conditional", "workbook": wb, "range": "C2:C7",
               "rule": "color_scale"}),
        json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:D7", "kind": "bar"}),
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:D7",
               "rows": "Region", "sum_field": "Qty"}),
    ];
    let before = kit.fake.excel_requests().len();
    for s in &steps {
        let mut single = s.clone();
        single["read_only"] = json!(true);
        let why = call(&kit, single).unwrap_err();
        assert!(why.contains("read_only"), "{why}");
        let why = call(&kit, json!({"read_only": true, "actions": [s]})).unwrap_err();
        assert!(why.contains("read_only"), "{why}");
        let mut walled = s.clone();
        walled["only_apps"] = json!(["notepad.exe"]);
        let why = call(&kit, walled).unwrap_err();
        assert!(why.contains("excel.exe"), "{why}");
    }
    let dir = kit.computer.config().paths().dir().to_path_buf();
    crate::safety::lock(&dir, "playing").unwrap();
    for s in &steps {
        let why = call(&kit, s.clone()).unwrap_err();
        assert!(why.contains("gaming lock"), "{why}");
    }
    crate::safety::unlock(&dir).unwrap();
    assert_eq!(
        kit.fake.excel_requests().len(),
        before,
        "none reached Excel"
    );
    // with excel.exe on the allowlist they go through
    for s in &steps {
        let mut ok = s.clone();
        ok["only_apps"] = json!(["excel.exe"]);
        assert!(call(&kit, ok.clone()).is_ok(), "{ok}");
    }
    // and the owner's switch still governs all three
    let off = Kit::with_env(MapEnv::new().with("HEKATON_AUTO_SHOT", "0"));
    for s in &steps {
        assert!(call(&off, s.clone()).unwrap_err().contains("power channel"));
    }
}

#[test]
fn bad_arguments_to_the_new_ops_are_refused_before_excel_hears_of_them() {
    let kit = kit();
    let wb = sales_book(&kit);
    let before = kit.fake.excel_requests().len();
    let fc =
        json!({"action": "excel", "op": "format_conditional", "workbook": wb, "range": "C2:C7"});
    let chart = json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:D7"});
    let pivot = json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:D7",
                       "rows": "Region", "sum_field": "Qty"});
    let cases = [
        (
            step(
                fc.clone(),
                json!({"rule": "greater_than", "value": 1, "color": "red"}),
            ),
            "#RRGGBB",
        ),
        (step(fc.clone(), json!({"rule": "sparkle"})), "greater_than"),
        (step(fc.clone(), json!({})), "needs rule"),
        (
            step(
                fc.clone(),
                json!({"rule": "greater_than", "color": "#FF0000"}),
            ),
            "needs value",
        ),
        (
            step(fc.clone(), json!({"rule": "greater_than", "value": 1})),
            "needs color",
        ),
        (
            step(
                fc.clone(),
                json!({"rule": "between", "value": 1, "color": "#FF0000"}),
            ),
            "needs value2",
        ),
        (
            step(
                fc.clone(),
                json!({"rule": "less_than", "value": "many", "color": "#FF0000"}),
            ),
            "float",
        ),
        (
            step(
                fc.clone(),
                json!({"range": "Sheet1!C2", "rule": "color_scale"}),
            ),
            "needs range",
        ),
        (
            step(chart.clone(), json!({"kind": "donut"})),
            "column, line, bar, pie",
        ),
        (step(chart.clone(), json!({})), "needs kind"),
        (
            step(chart.clone(), json!({"kind": "pie", "anchor": "nowhere"})),
            "anchor",
        ),
        (
            step(chart.clone(), json!({"kind": "pie", "range": "A1:D1"})),
            "header row",
        ),
        (
            step(chart.clone(), json!({"kind": "pie", "range": "A:D"})),
            "needs range",
        ),
        (
            step(pivot.clone(), json!({"sum_field": ""})),
            "needs sum_field",
        ),
        (step(pivot.clone(), json!({"rows": ""})), "needs rows"),
        (
            step(pivot.clone(), json!({"columns": "region"})),
            "different",
        ),
        (step(pivot.clone(), json!({"name": "a/b"})), "1 to 31"),
        (
            step(pivot.clone(), json!({"range": "A1:A7"})),
            "two columns",
        ),
        (step(pivot.clone(), json!({"range": "A1:D1"})), "header row"),
    ];
    for (s, needle) in cases {
        let why = call(&kit, s.clone()).unwrap_err();
        assert!(why.contains(needle), "{needle} not in {why} for {s}");
    }
    assert_eq!(kit.fake.excel_requests().len(), before);
}

#[test]
fn a_pivot_header_that_is_not_there_is_named_and_no_cell_data_comes_back() {
    let kit = kit();
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [
            ["SECRET-HEAD-A", "SECRET-HEAD-B", "SECRET-HEAD-C"],
            ["SECRET-ROW", 1, 2]
        ]}),
    )
    .unwrap();
    for (extra, asked) in [
        (
            json!({"rows": "Regon", "sum_field": "SECRET-HEAD-B"}),
            "Regon",
        ),
        (
            json!({"rows": "SECRET-HEAD-A", "sum_field": "Qtty"}),
            "Qtty",
        ),
        (
            json!({"rows": "SECRET-HEAD-A", "sum_field": "SECRET-HEAD-B", "columns": "Prod"}),
            "Prod",
        ),
    ] {
        let why = call(
            &kit,
            step(
                json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:C2"}),
                extra,
            ),
        )
        .unwrap_err();
        assert!(why.contains(&format!("{asked:?}")), "{why}");
        assert!(why.contains("A1:C2"), "{why}");
        // the error never lists the headers or any cell that is there
        let rest = why.replace(asked, "");
        assert!(!rest.contains("SECRET"), "{why}");
    }
    // nothing was left behind: no stray sheet
    let sheets = call(
        &kit,
        json!({"action": "excel", "op": "add_sheet", "workbook": wb, "name": "Check"}),
    )
    .unwrap();
    assert!(sheets.contains("(sheets: Sheet1, Check)"), "{sheets}");
}

#[test]
fn the_audit_log_for_the_new_ops_has_no_cell_text_headers_or_titles() {
    let kit = kit();
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [
            ["SECRET-REGION", "SECRET-QTY"],
            ["SECRET-EAST", 4],
            ["SECRET-WEST", 6]
        ]}),
    )
    .unwrap();
    call(
        &kit,
        json!({"action": "excel", "op": "format_conditional", "workbook": wb, "range": "B2:B3",
               "rule": "greater_than", "value": 5, "color": "#FFC7CE"}),
    )
    .unwrap();
    call(
        &kit,
        json!({"action": "excel", "op": "chart", "workbook": wb, "range": "A1:B3", "kind": "column",
               "title": "SECRET-TITLE"}),
    )
    .unwrap();
    call(
        &kit,
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:B3",
               "rows": "SECRET-REGION", "sum_field": "SECRET-QTY"}),
    )
    .unwrap();
    // a refused pivot logs through the failure route, which logs the arguments
    let _ = call(
        &kit,
        json!({"action": "excel", "op": "pivot", "workbook": wb, "range": "A1:B3",
               "rows": "SECRET-REGION", "sum_field": "SECRET-GONE", "name": "Second"}),
    );
    kit.clock.advance(std::time::Duration::from_secs(2));
    call(
        &kit,
        json!({"actions": [
            {"action": "excel", "op": "chart", "workbook": wb, "range": "A1:B3", "kind": "pie",
             "title": "SECRET-TITLE-2"}
        ]}),
    )
    .unwrap();
    let log = std::fs::read_to_string(kit.computer.config().paths().audit_log()).unwrap();
    // the one place a name the caller asked for may appear is the refusal text, and that name is
    // not on the sheet; every header, title and cell that IS on the sheet stays out
    assert!(!log.replace("SECRET-GONE", "").contains("SECRET"), "{log}");
    for needle in [
        r#""op": "format_conditional""#,
        r#""op": "chart""#,
        r#""op": "pivot""#,
        r#""rule": "greater_than""#,
        r#""kind": "column""#,
        r#""range": "A1:B3""#,
    ] {
        assert!(log.contains(needle), "{needle} not in {log}");
    }
}

#[test]
fn written_text_is_literal_and_only_a_leading_equals_is_a_formula() {
    let kit = kit();
    let wb = new_book(&kit);
    call(
        &kit,
        json!({"action": "excel", "op": "write", "workbook": wb, "values": [
            ["007", "1/2", "TRUE", "-x", 7, true, "=A1&B1"]
        ]}),
    )
    .unwrap();
    let read = call(
        &kit,
        json!({"action": "excel", "op": "read", "workbook": wb, "range": "A1:F1", "formulas": true}),
    )
    .unwrap();
    assert!(
        read.contains(r#""values":[["007","1/2","TRUE","-x",7,true]]"#),
        "{read}"
    );
}
