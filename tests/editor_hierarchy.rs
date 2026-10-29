#[cfg(unix)]
#[path = "support/lsp_client.rs"]
pub mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod hierarchy {
    use super::{lsp_client::LspClient, support::Must};
    use serde_json::{Value, json};
    use std::{
        collections::BTreeSet,
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
    };
    use tempfile::tempdir;
    use url::Url;

    fn client(root: &Path) -> LspClient {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).must_be("create compiler directory");
        let bend = bin.join("bend");
        fs::write(&bend, "#!/bin/sh\nif [ \"$1\" = base ]; then printf 'def builtin() -> U32:\\n  1\\n'; fi\nexit 0\n")
            .must_be("write compiler fixture");
        fs::set_permissions(&bend, fs::Permissions::from_mode(0o755))
            .must_be("compiler permissions");
        let paths = std::iter::once(bin)
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .collect::<Vec<_>>();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .env("PATH", std::env::join_paths(paths).must_be("test PATH"))
            .env("BEND_LIB", root.join("hub"))
            .stderr(Stdio::null());
        let mut client = LspClient::spawn(command);
        client.initialize(root);
        client
    }

    fn uri(path: &Path) -> String {
        Url::from_file_path(path).must_be("file URI").into()
    }

    fn open(client: &mut LspClient, uri: &str, text: &str) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":text}}),
        );
    }

    fn prepare(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
        let response = client.request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}}),
        );
        response["result"][0].clone()
    }

    const SHARED_CALLER: &str = "def f(x: U32) -> U32:\n  x\ndef g(x: U32) -> U32:\n  f(x)\n";
    const ROOT_A: &str = "import ./shared.bend as Shared\ndef a(x: U32) -> U32:\n  Shared.g(x)\n";
    const ROOT_B: &str = "import ./shared.bend as Shared\ndef b(x: U32) -> U32:\n  Shared.g(x)\n";

    // Two independent entries load the same dependency, where `g` calls `f`.
    // Both caller and call site are physical facts of the dependency, so the
    // caller must be reported once and stay reachable through its own handle.
    #[test]
    fn incoming_calls_present_one_physical_shared_caller_call() {
        let temp = tempdir().must_be("temporary workspace");
        let root = temp.path();
        let shared_path = root.join("shared.bend");
        let a_path = root.join("a.bend");
        let b_path = root.join("b.bend");
        fs::write(&shared_path, SHARED_CALLER).must_be("write shared dependency");
        fs::write(&a_path, ROOT_A).must_be("write first entry");
        fs::write(&b_path, ROOT_B).must_be("write second entry");
        let shared_uri = uri(&shared_path);
        let mut client = client(root);
        open(&mut client, &shared_uri, SHARED_CALLER);
        open(&mut client, &uri(&a_path), ROOT_A);
        open(&mut client, &uri(&b_path), ROOT_B);
        let item = prepare(&mut client, &shared_uri, 0, 4);
        assert_eq!(item["name"], "f", "prepare the shared callee: {item}");
        let incoming = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let calls = incoming["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            calls.len(),
            1,
            "one physical call must be presented once regardless of loaded entries: {incoming}"
        );
        let call = &calls[0];
        assert_eq!(call["from"]["name"], "g");
        assert_eq!(
            call["from"]["uri"], shared_uri,
            "caller lives in the dependency"
        );
        assert_eq!(
            call["from"]["range"],
            json!({"start":{"line":2,"character":0},"end":{"line":3,"character":6}}),
            "the caller item keeps its whole declaration range"
        );
        assert_eq!(
            call["from"]["selectionRange"],
            json!({"start":{"line":2,"character":4},"end":{"line":2,"character":5}}),
            "the caller item selects the caller name"
        );
        assert_eq!(
            call["fromRanges"],
            json!([{"start":{"line":3,"character":2},"end":{"line":3,"character":3}}]),
            "the call site is the physical `f` reference inside `g`"
        );
        assert_eq!(call["from"]["data"]["schema"], 1);
        assert_eq!(
            call["from"]["data"]["rootUri"], item["data"]["rootUri"],
            "the caller handle stays in the entry context that prepared the callee"
        );
        assert_eq!(call["from"]["data"]["view"], item["data"]["view"]);
        let outgoing = client.request("callHierarchy/outgoingCalls", json!({"item":call["from"]}));
        let edges = outgoing["result"]
            .as_array()
            .must_be("outgoing calls must return an array");
        assert_eq!(
            edges.len(),
            1,
            "the reported caller handle must resolve in its own entry: {outgoing}"
        );
        assert_eq!(edges[0]["to"]["name"], "f");
        assert_eq!(edges[0]["to"]["uri"], shared_uri);
        assert_eq!(edges[0]["fromRanges"], call["fromRanges"]);
        client.finish();
    }

    // Mirrors the pinned compiler witness for an imported law filled by each
    // entry. The fill requires an unannotated implementation parameter set.
    const SHARED_CLAIM: &str =
        "import Base\n\nlaw g:\n  for x: U32\n  U32\n\ndef f(x: U32) -> U32:\n  x\n";
    const CLAIM_A: &str = "import Base\nimport ./shared.bend as Shared\n\ndef Shared.g(x):\n  Shared.f(x)\n\ndef main() -> U32:\n  Shared.g(1)\n";
    const CLAIM_B: &str = "import Base\nimport ./shared.bend as Shared\n\ndef Shared.g(x):\n  Shared.f(x)\n\ndef main() -> U32:\n  Shared.g(1)\n";

    // The same claim is implemented once per entry. Each implementation is a
    // distinct physical caller, so the entry-dependent meaning must survive as
    // separate incoming calls instead of collapsing onto the shared claim.
    #[test]
    fn incoming_calls_keep_entry_specific_law_implementations_distinct() {
        let temp = tempdir().must_be("temporary law workspace");
        let root = temp.path();
        let shared_path = root.join("shared.bend");
        let a_path = root.join("a.bend");
        let b_path = root.join("b.bend");
        fs::write(&shared_path, SHARED_CLAIM).must_be("write shared claim");
        fs::write(&a_path, CLAIM_A).must_be("write first implementation");
        fs::write(&b_path, CLAIM_B).must_be("write second implementation");
        let shared_uri = uri(&shared_path);
        let a_uri = uri(&a_path);
        let b_uri = uri(&b_path);
        let mut client = client(root);
        open(&mut client, &shared_uri, SHARED_CLAIM);
        open(&mut client, &a_uri, CLAIM_A);
        open(&mut client, &b_uri, CLAIM_B);
        let item = prepare(&mut client, &shared_uri, 6, 4);
        assert_eq!(item["name"], "f", "prepare the shared callee: {item}");
        let incoming = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let calls = incoming["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            calls.len(),
            2,
            "each entry's law implementation is a separate caller: {incoming}"
        );
        for (call, caller_uri) in calls.iter().zip([a_uri, b_uri]) {
            assert_eq!(call["from"]["name"], "g", "the claim names the caller");
            assert_eq!(call["from"]["uri"], caller_uri);
            assert_eq!(
                call["from"]["range"],
                json!({"start":{"line":3,"character":0},"end":{"line":4,"character":13}}),
                "each caller item spans its own implementation declaration"
            );
            assert_eq!(
                call["from"]["selectionRange"],
                json!({"start":{"line":3,"character":4},"end":{"line":3,"character":12}}),
                "the implementation selects its qualified declaration name"
            );
            assert_eq!(
                call["fromRanges"],
                json!([{"start":{"line":4,"character":9},"end":{"line":4,"character":10}}]),
                "the call site is the physical `f` reference in that implementation"
            );
            assert_eq!(call["from"]["data"]["rootUri"], caller_uri);
            let outgoing =
                client.request("callHierarchy/outgoingCalls", json!({"item":call["from"]}));
            let edges = outgoing["result"]
                .as_array()
                .must_be("outgoing calls must return an array");
            assert_eq!(
                edges.len(),
                1,
                "each entry-specific caller handle must resolve: {outgoing}"
            );
            assert_eq!(edges[0]["to"]["name"], "f");
            assert_eq!(edges[0]["to"]["uri"], shared_uri);
            assert_eq!(
                edges[0]["to"]["selectionRange"],
                json!({"start":{"line":6,"character":4},"end":{"line":6,"character":5}})
            );
            assert_eq!(edges[0]["fromRanges"], call["fromRanges"]);
        }
        client.finish();
    }

    const SHARED_CONTEXT: &str = "import Base\n\nlaw helper:\n  for x: U32\n  U32\n\ndef f(x: U32) -> U32:\n  x\n\ndef g(x: U32) -> U32:\n  helper(f(x))\n";
    const HELPER_A: &str = "import Base\nimport ./shared.bend as Shared\n\ndef Shared.helper(x):\n  x\n\ndef main() -> U32:\n  Shared.helper(1)\n";
    const HELPER_B: &str = "import Base\nimport ./shared.bend as Shared\n\ndef Shared.helper(x):\n  x\n\ndef main() -> U32:\n  Shared.helper(1)\n";

    // The caller body and call site are the same physical `g` in every entry,
    // but `g` also calls the `helper` claim, whose implementation each entry
    // supplies in a different file. The incoming call site is identical, so
    // the entry-dependent meaning of the caller must survive as separate
    // handles whose outgoing calls resolve their own implementation.
    #[test]
    fn incoming_calls_keep_entry_specific_callee_meaning_for_one_physical_caller() {
        let temp = tempdir().must_be("temporary law workspace");
        let root = temp.path();
        let shared_path = root.join("shared.bend");
        let a_path = root.join("a.bend");
        let b_path = root.join("b.bend");
        fs::write(&shared_path, SHARED_CONTEXT).must_be("write shared context");
        fs::write(&a_path, HELPER_A).must_be("write first helper implementation");
        fs::write(&b_path, HELPER_B).must_be("write second helper implementation");
        let shared_uri = uri(&shared_path);
        let a_uri = uri(&a_path);
        let b_uri = uri(&b_path);
        let mut client = client(root);
        open(&mut client, &shared_uri, SHARED_CONTEXT);
        open(&mut client, &a_uri, HELPER_A);
        open(&mut client, &b_uri, HELPER_B);
        let item = prepare(&mut client, &shared_uri, 6, 4);
        assert_eq!(item["name"], "f", "prepare the shared callee: {item}");
        let incoming = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let calls = incoming["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            calls.len(),
            3,
            "one physical caller with entry-dependent callees keeps every meaning: {incoming}"
        );
        let mut roots = BTreeSet::new();
        let mut views = BTreeSet::new();
        for call in calls {
            assert_eq!(call["from"]["name"], "g");
            assert_eq!(
                call["from"]["uri"], shared_uri,
                "the caller is one physical declaration in the shared file"
            );
            assert_eq!(
                call["from"]["range"],
                json!({"start":{"line":9,"character":0},"end":{"line":10,"character":14}}),
                "the caller item keeps its whole declaration range"
            );
            assert_eq!(
                call["from"]["selectionRange"],
                json!({"start":{"line":9,"character":4},"end":{"line":9,"character":5}}),
                "the caller item selects the caller name"
            );
            assert_eq!(
                call["fromRanges"],
                json!([{"start":{"line":10,"character":9},"end":{"line":10,"character":10}}]),
                "the incoming call site is the same physical `f` reference"
            );
            let root_uri = call["from"]["data"]["rootUri"]
                .as_str()
                .must_be("caller handle root URI")
                .to_owned();
            roots.insert(root_uri.clone());
            views.insert(
                call["from"]["data"]["view"]
                    .as_u64()
                    .must_be("caller handle view serial"),
            );
            let outgoing =
                client.request("callHierarchy/outgoingCalls", json!({"item":call["from"]}));
            let edges = outgoing["result"]
                .as_array()
                .must_be("outgoing calls must return an array");
            assert_eq!(
                edges.len(),
                2,
                "the caller expands to both callees in its own entry: {outgoing}"
            );
            let callee = edges
                .iter()
                .find(|edge| edge["to"]["name"] == "f")
                .must_be("outgoing call to the shared callee");
            assert_eq!(callee["to"]["uri"], shared_uri);
            assert_eq!(
                callee["to"]["selectionRange"],
                json!({"start":{"line":6,"character":4},"end":{"line":6,"character":5}})
            );
            assert_eq!(
                callee["fromRanges"],
                json!([{"start":{"line":10,"character":9},"end":{"line":10,"character":10}}])
            );
            let helper = edges
                .iter()
                .find(|edge| edge["to"]["name"] == "helper")
                .must_be("outgoing call to the helper claim");
            assert_eq!(
                helper["fromRanges"],
                json!([{"start":{"line":10,"character":2},"end":{"line":10,"character":8}}])
            );
            assert_eq!(
                helper["to"]["uri"], root_uri,
                "each handle resolves the helper implementation of its own entry"
            );
            assert_eq!(
                helper["to"]["selectionRange"],
                if root_uri == shared_uri {
                    json!({"start":{"line":2,"character":4},"end":{"line":2,"character":10}})
                } else {
                    json!({"start":{"line":3,"character":4},"end":{"line":3,"character":17}})
                },
                "the helper target is the claim for the shared entry and the definition otherwise"
            );
        }
        assert_eq!(
            roots.into_iter().collect::<Vec<_>>(),
            [a_uri, b_uri, shared_uri],
            "every loaded entry context is preserved"
        );
        assert_eq!(
            views.len(),
            3,
            "each caller handle keeps its own entry identity"
        );
        client.finish();
    }

    const SHARED_SECOND_HOP: &str = "import Base\n\nlaw helper:\n  for x: U32\n  U32\n\ndef f(x: U32) -> U32:\n  helper(x)\n\ndef g(x: U32) -> U32:\n  f(x)\n";

    // The same physical `g` calls `f`, and `f` itself calls the `helper` claim
    // filled differently per entry. The caller's immediate outgoing projection
    // is identical across entries, so only a full continuation comparison may
    // decide equivalence: every entry must survive and its second hop must
    // resolve its own helper implementation.
    #[test]
    fn incoming_calls_keep_second_hop_callee_meaning_for_one_physical_caller() {
        let temp = tempdir().must_be("temporary law workspace");
        let root = temp.path();
        let shared_path = root.join("shared.bend");
        let a_path = root.join("a.bend");
        let b_path = root.join("b.bend");
        fs::write(&shared_path, SHARED_SECOND_HOP).must_be("write shared second hop");
        fs::write(&a_path, HELPER_A).must_be("write first helper implementation");
        fs::write(&b_path, HELPER_B).must_be("write second helper implementation");
        let shared_uri = uri(&shared_path);
        let a_uri = uri(&a_path);
        let b_uri = uri(&b_path);
        let mut client = client(root);
        open(&mut client, &shared_uri, SHARED_SECOND_HOP);
        open(&mut client, &a_uri, HELPER_A);
        open(&mut client, &b_uri, HELPER_B);
        let item = prepare(&mut client, &shared_uri, 6, 4);
        assert_eq!(item["name"], "f", "prepare the shared callee: {item}");
        let incoming = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let calls = incoming["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            calls.len(),
            3,
            "a second-hop callee meaning keeps every entry variant: {incoming}"
        );
        let mut roots = BTreeSet::new();
        for call in calls {
            assert_eq!(call["from"]["name"], "g");
            assert_eq!(call["from"]["uri"], shared_uri);
            assert_eq!(
                call["from"]["range"],
                json!({"start":{"line":9,"character":0},"end":{"line":10,"character":6}})
            );
            assert_eq!(
                call["from"]["selectionRange"],
                json!({"start":{"line":9,"character":4},"end":{"line":9,"character":5}})
            );
            assert_eq!(
                call["fromRanges"],
                json!([{"start":{"line":10,"character":2},"end":{"line":10,"character":3}}]),
                "the incoming call site is the same physical `f` reference"
            );
            let root_uri = call["from"]["data"]["rootUri"]
                .as_str()
                .must_be("caller handle root URI")
                .to_owned();
            roots.insert(root_uri.clone());
            let outgoing =
                client.request("callHierarchy/outgoingCalls", json!({"item":call["from"]}));
            let edges = outgoing["result"]
                .as_array()
                .must_be("first hop must return an array");
            assert_eq!(
                edges.len(),
                1,
                "the first hop is identical in every entry: {outgoing}"
            );
            assert_eq!(edges[0]["to"]["name"], "f");
            assert_eq!(edges[0]["to"]["uri"], shared_uri);
            assert_eq!(
                edges[0]["to"]["selectionRange"],
                json!({"start":{"line":6,"character":4},"end":{"line":6,"character":5}})
            );
            assert_eq!(
                edges[0]["fromRanges"],
                json!([{"start":{"line":10,"character":2},"end":{"line":10,"character":3}}])
            );
            let second = client.request(
                "callHierarchy/outgoingCalls",
                json!({"item":edges[0]["to"]}),
            );
            let hops = second["result"]
                .as_array()
                .must_be("second hop must return an array");
            assert_eq!(
                hops.len(),
                1,
                "the second hop resolves the callee in that entry's context: {second}"
            );
            assert_eq!(hops[0]["to"]["name"], "helper");
            assert_eq!(
                hops[0]["fromRanges"],
                json!([{"start":{"line":7,"character":2},"end":{"line":7,"character":8}}])
            );
            assert_eq!(
                hops[0]["to"]["uri"], root_uri,
                "the second hop keeps the entry's helper implementation"
            );
            assert_eq!(
                hops[0]["to"]["selectionRange"],
                if root_uri == shared_uri {
                    json!({"start":{"line":2,"character":4},"end":{"line":2,"character":10}})
                } else {
                    json!({"start":{"line":3,"character":4},"end":{"line":3,"character":17}})
                }
            );
        }
        assert_eq!(
            roots.into_iter().collect::<Vec<_>>(),
            [a_uri, b_uri, shared_uri],
            "every entry context with distinct second-hop meaning is kept"
        );
        client.finish();
    }

    const SHARED_CYCLE: &str = "import Base\n\nlaw f:\n  for x: U32\n  U32\n\nlaw g:\n  for x: U32\n  U32\n\ndef f(x):\n  g(x)\n\ndef g(x):\n  f(x)\n";

    // Two entries load one recursive dependency: `g` calls `f` and `f` calls
    // `g` again. Every entry resolves the cycle identically, so the physical
    // caller must be reported once and its handle must keep closing the same
    // cycle edge for edge.
    #[test]
    fn incoming_calls_merge_identical_recursive_contexts() {
        let temp = tempdir().must_be("temporary workspace");
        let root = temp.path();
        let shared_path = root.join("shared.bend");
        let a_path = root.join("a.bend");
        let b_path = root.join("b.bend");
        fs::write(&shared_path, SHARED_CYCLE).must_be("write shared cycle");
        fs::write(&a_path, ROOT_A).must_be("write first entry");
        fs::write(&b_path, ROOT_B).must_be("write second entry");
        let shared_uri = uri(&shared_path);
        let mut client = client(root);
        open(&mut client, &shared_uri, SHARED_CYCLE);
        open(&mut client, &uri(&a_path), ROOT_A);
        open(&mut client, &uri(&b_path), ROOT_B);
        let item = prepare(&mut client, &shared_uri, 10, 4);
        assert_eq!(item["name"], "f", "prepare the recursive callee: {item}");
        let incoming = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let calls = incoming["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            calls.len(),
            1,
            "identical recursive entries present one physical caller: {incoming}"
        );
        let call = &calls[0];
        assert_eq!(call["from"]["name"], "g");
        assert_eq!(call["from"]["uri"], shared_uri);
        assert_eq!(
            call["from"]["range"],
            json!({"start":{"line":13,"character":0},"end":{"line":14,"character":6}})
        );
        assert_eq!(
            call["from"]["selectionRange"],
            json!({"start":{"line":13,"character":4},"end":{"line":13,"character":5}})
        );
        assert_eq!(
            call["fromRanges"],
            json!([{"start":{"line":14,"character":2},"end":{"line":14,"character":3}}])
        );
        assert_eq!(
            call["from"]["data"]["rootUri"], item["data"]["rootUri"],
            "the merged caller stays in the prepared entry context"
        );
        assert_eq!(call["from"]["data"]["view"], item["data"]["view"]);
        let first = client.request("callHierarchy/outgoingCalls", json!({"item":call["from"]}));
        let to_f = first["result"]
            .as_array()
            .must_be("first cycle hop must return an array");
        assert_eq!(
            to_f.len(),
            1,
            "the recursive caller has one outgoing edge: {first}"
        );
        assert_eq!(to_f[0]["to"]["name"], "f");
        assert_eq!(to_f[0]["to"]["uri"], shared_uri);
        assert_eq!(
            to_f[0]["to"]["range"],
            json!({"start":{"line":10,"character":0},"end":{"line":11,"character":6}})
        );
        assert_eq!(
            to_f[0]["to"]["selectionRange"],
            json!({"start":{"line":10,"character":4},"end":{"line":10,"character":5}})
        );
        assert_eq!(
            to_f[0]["fromRanges"],
            json!([{"start":{"line":14,"character":2},"end":{"line":14,"character":3}}])
        );
        let second = client.request("callHierarchy/outgoingCalls", json!({"item":to_f[0]["to"]}));
        let to_g = second["result"]
            .as_array()
            .must_be("second cycle hop must return an array");
        assert_eq!(
            to_g.len(),
            1,
            "the cycle continues through the callee: {second}"
        );
        assert_eq!(to_g[0]["to"]["name"], "g");
        assert_eq!(to_g[0]["to"]["uri"], shared_uri);
        assert_eq!(
            to_g[0]["to"], call["from"],
            "the cycle returns the same caller item"
        );
        assert_eq!(
            to_g[0]["fromRanges"],
            json!([{"start":{"line":11,"character":2},"end":{"line":11,"character":3}}])
        );
        let closed = client.request("callHierarchy/outgoingCalls", json!({"item":to_g[0]["to"]}));
        let to_f_again = closed["result"]
            .as_array()
            .must_be("closed cycle must return an array");
        assert_eq!(
            to_f_again.len(),
            1,
            "the visited-pair traversal keeps the cycle finite: {closed}"
        );
        assert_eq!(
            to_f_again[0]["to"], to_f[0]["to"],
            "the closed edge points at the same callee item"
        );
        assert_eq!(to_f_again[0]["fromRanges"], to_f[0]["fromRanges"]);
        client.finish();
    }
}
