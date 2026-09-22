# Wire BoundedDbExecutor (AI B → AI A)

Module is on the branch: `apps/srs_license_server/src/db_exec.rs`.
It is not compiled until `main.rs` includes it. Do not merge until this wiring exists and `cargo test -p srs_license_server` runs.

## main.rs

```rust
mod db_exec;
mod security;
```

```rust
struct Database {
    conn: Mutex<Connection>,
    exec: db_exec::BoundedDbExecutor,
}
```

In `Database::open`:

```rust
Ok(Self {
    conn: Mutex::new(conn),
    exec: db_exec::BoundedDbExecutor::new(),
})
```

Keep every existing method synchronous and parameterized (`params![]`). Handlers become:

```rust
async fn issue_json(...) -> AppResult<Json<IssueKeyResponse>> {
    request.registrant_ip = Some(addr.ip().to_string());
    let db = state.db.clone();
    let response = db.exec.run({
        let db = db.clone();
        move || db.issue_license(&request)
    }).await?;
    Ok(Json(response))
}
```

Same pattern for `verify_json`, `admin_snapshot_json`, `confirm_request`, mutations.
Never hold `Mutex<Connection>` across `.await`.
Never call `spawn_blocking` from a handler except through `BoundedDbExecutor` (permit count = 1).

## Tests already in db_exec.rs

- value return
- error propagation / permit release
- four concurrent jobs peak inflight == 1

A still owns HTTP contract tests (401/403/issue-without-auth) once a test harness can bind the router.
