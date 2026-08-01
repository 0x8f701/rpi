use pi_coding::{
    ApplicationEvent, SelectionHit, SelectionKind, SelectionPlan, SelectionSource,
};

fn plan() -> SelectionPlan {
    SelectionPlan {
        request: "review Rust".to_owned(),
        skills: vec![SelectionHit {
            kind: SelectionKind::Skill,
            name: "rust-review".to_owned(),
            description: "Review Rust code".to_owned(),
            score: 1_320,
            source: SelectionSource::Deterministic,
            reasons: vec!["exact name phrase".to_owned()],
            location: Some("skill://rust-review".to_owned()),
            trusted: true,
        }],
        ..SelectionPlan::default()
    }
}

#[test]
fn application_selection_event_serializes_full_explanation() {
    let value = serde_json::to_value(ApplicationEvent::Selection(plan())).expect("serialize event");
    assert_eq!(value["type"], "selection");
    assert_eq!(value["selection"]["skills"][0]["name"], "rust-review");
    assert_eq!(value["selection"]["skills"][0]["score"], 1_320);
    assert_eq!(
        value["selection"]["skills"][0]["reasons"][0],
        "exact name phrase"
    );
    assert_eq!(
        value["selection"]["skills"][0]["location"],
        "skill://rust-review"
    );
}
