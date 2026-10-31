use bend2_lsp::analysis::{CompletionKind, DocumentSnapshot, Revision, module_completion_items};

fn main() {
    let constructors = "type Shape is Data:\n  NeedleCtor{}\n";
    let mut plain = String::new();
    for index in 0..32 {
        plain.push_str(&format!("def noise{index:02}() -> U32:\n  {index}\n"));
    }
    let expected: Vec<String> = (0..32).map(|index| format!("needle{index:02}")).collect();
    for name in &expected {
        plain.push_str(&format!("def {name}() -> U32:\n  0\n"));
    }
    let with_constructors = format!("{constructors}{plain}");
    let expected_labels: Vec<&str> = expected.iter().map(String::as_str).collect();
    for source in [plain, with_constructors] {
        let snapshot = DocumentSnapshot::new(Revision(1), source);
        for query in ["needle", "NEEDLE"] {
            let items = module_completion_items(&snapshot, query);
            let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
            assert_eq!(labels, expected_labels);
            assert!(items.iter().all(|item| matches!(item.kind, CompletionKind::Function)));
        }
        for query in ["zzzz", "é"] {
            assert!(module_completion_items(&snapshot, query).is_empty());
        }
    }
    let constructor_only = DocumentSnapshot::new(Revision(2), constructors.to_owned());
    assert!(module_completion_items(&constructor_only, "needle").is_empty());
    let empty = DocumentSnapshot::new(Revision(3), String::new());
    assert!(module_completion_items(&empty, "").is_empty());
    println!("module completion API: 10 behavioral scenarios passed; ordered 32-result growth, late first match, both constructor-presence paths, ASCII case folding, Unicode no-match and empty input");
}
