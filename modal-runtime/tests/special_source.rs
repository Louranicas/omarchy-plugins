use modal_runtime::{targets::NativeTarget, workspace_move::observe};
use serde_json::json;
#[test]
fn special_source_must_be_refused_by_numeric_only_route() {
    let clients = json!([{"stableId":"ab","address":"0x1","mapped":true,"hidden":false,"pinned":false,"swallowing":"0x0","grouped":[],"fullscreen":0,"monitor":0,"workspace":{"id":-99,"name":"special:stash"}}]);
    let workspaces = json!([{"id":-99,"name":"special:stash","monitorID":0},{"id":2,"name":"2","monitorID":0},{"id":3,"name":"3","monitorID":0}]);
    let active = json!({"id":2,"name":"2","monitorID":0});
    let result = observe(
        &clients,
        &workspaces,
        &active,
        &NativeTarget::new("fixture", "ab").unwrap(),
        3,
        true,
    );
    assert!(result.is_err(), "scratchpad source admitted: {result:?}");
}
