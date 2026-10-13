# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`rust-web-frame` is a Rust web framework ported from the design of
`../go-web-frame`. The Go original is built on Gin + GORM + Viper + Zap; this one is built
on axum + sea-orm + a hand-written config layer + `tracing`. Read
[`README.md`](README.md) for the user-facing pitch; read this file before changing the
architecture.

## Commands

```bash
cargo test                        # unit + integration (`tests/`) + doc tests
cargo test --lib                  # unit tests only
cargo test --test web_layer       # drive the real axum router with tower::oneshot
cargo clippy --all-targets        # must stay warning-free
cargo fmt
cargo run --example helloworld    # http://localhost:19009/
cargo run --example rest          # controller + service + runner
cargo run --example model         # entities/queries/transactions, in-memory SQLite (port 19011)
cargo run --example withmeta      # declarative routing
cargo test --features mysql       # build against another driver
```

Feature flags: `sqlite` (default), `mysql`, `postgres`.

## Architecture

```
src/
  lib.rs          crate docs, module list, prelude
  frame.rs        Builder + WebFrame — the application lifecycle
  core/           the component model
    context.rs      Context — the TypeId-keyed DI container, and route registration
    service.rs      Service / Rest / Runner / Filter-init traits, erasure, wire fns
    registry.rs     Registration — one component, as registered but not yet initialised
    model_group.rs  ModelGroup — models sharing a connection
    rest_group.rs   RestGroup — controllers on their own port
    runtime.rs      Runtime — binds servers, spawns runners, waits for shutdown
  web/            the HTTP layer
    server.rs       Server — route table -> axum Router, dispatch, 404/405
    routes.rs       Routes — the route table, and each registration method
    route.rs        Route, HandlerMeta, MetaOption, RouteHandle, path translation
    filter.rs       Filter, FilterChain, handler(), FnFilter
    request.rs      Request
    response.rs     Response, ResponseBody, IntoResponse
    converter.rs    Converter, DefaultConverter
    error.rs        Error (HTTP code + business code), Result alias
    message.rs      Message — the {code,data,msg,type} envelope
    config.rs       ServerConfig
    multipart.rs    MultipartForm, UploadedFile, streamed parsing, temp-file spilling
    file.rs sse.rs ws.rs
  model/          the ORM layer
    mod.rs          Model<E>, EntryModel<E>, TableEntity/Table, define_model!, Transaction
    query.rs        Query<E>, Update<E>, Delete<E>, Aggregate<E>
  db/             Database (pool *or* transaction), DbConfig, connect, transaction
  config/         Config — dotted-key lookup, auto-loading, deep merge
  log/            tracing setup
  value/          serde_json helpers (ObjectExt, get_path, merge)
  util/           Page/Paged, string helpers
```

### The lifecycle

`Builder` (frame.rs) accumulates `Registration`s, model groups, rest groups and routes.
`WebFrame::start` then:

1. initialises logging, reads `web.server` into a `ServerConfig`
2. creates the default `web::Server`, merges the builder's routes into it
3. creates the `Context`, attaches the database from the `db` config section
4. prepares each model group (creating tables), registering models into the `Context`
5. creates a `web::Server` per rest group
6. calls `init_all` on the registrations **in order**, wiring filters/converters/runners
   into the servers as each component becomes ready
7. runs the inline `Builder::routes` closures
8. validates every route table, then hands the servers and runners to `Runtime`

### The component model (`core/service.rs`)

Four component traits, each declaring its own `init(&self, ctx)`, so an `impl` block has
one method to override:

| Trait | Registered with | Extra method |
|---|---|---|
| `Service` | `Builder::service` | — |
| `Rest` | `Builder::rest` | — (registers routes in `init`) |
| `web::Filter` | `Builder::filter` | `handle` |
| `Runner` | `Builder::runner` | `run` |

`Converter` has no `init`; a converter that needs startup work implements `Service` too.

**Erasure.** Rust has no reflection, so `Registration` holds an
`Arc<dyn Any + Send + Sync>` plus three closures captured at registration time:
`InitFn` (calls the right trait's `init`), `WireFn` (downcasts to produce
`Arc<dyn Filter>` / `Arc<dyn Converter>` / `Arc<dyn Runner>`), and the `TypeId` the
component is looked up by.

Two invariants that are easy to break:

* **`init` must take `&self`, not `&mut self`.** Shared receivers are what let one
  instance be an `Arc` from the start, serve as a service *and* as a filter, and be
  initialised without exclusive access. Components that need startup state use
  `OnceLock`.
* **The registration key must be the user's type**, never an adapter's. Registering a
  filter as `AsFilter<Counter>` would make `ctx.service::<Counter>()` fail; there are
  regression tests for exactly this in `core/service.rs` and `tests/web_layer.rs`.

A component is only visible to components registered *after* it. Document that; do not
try to make lookups order-independent.

### Routing (`web/route.rs`, `web/server.rs`)

Routes are keyed by `(path, method)`. Paths are written in Gin syntax (`/users/:id`,
`/static/*filepath`) and translated by `to_axum_path` for matchit; `/{id}` also passes
through. `RouteHandle` wraps `Arc<Route>` and `with_meta` consumes it — idiomatic builder,
same chaining as `ctx.Get(...).WithMeta(...)` in the Go original.

`Server::validate` runs before binding and turns two mistakes into startup errors instead
of panics: duplicate `(path, method)` pairs, and path templates matchit rejects. It probes
each *distinct* path once, because several routes legitimately share one path.

Dispatch (`Server::router` + `dispatch`): read the body under `effective_max_body_size`,
extract path params and a pending `WebSocketUpgrade`, build a `Request`, run the
`FilterChain`, hand the result to the `Converter`. Requests that match no route go to
`fallback` (static files → SPA page → 404 envelope); requests whose path matched but
method did not go to the `MethodRouter` fallback and get a 405 envelope.

### Request bodies (`web/request.rs`, `web/multipart.rs`)

`Request::body` is an enum, not a `Bytes`:

* `Body::Buffered` — everything except a multipart upload, read under
  `max_body_size` while dispatching.
* `Body::Streaming` — a multipart body, deliberately left on the wire. Dispatch decides
  from the `Content-Type` and never touches it.

`multipart()` and `read_body()` each consume the stream at most once and cache their
result in a `tokio::sync::OnceCell`; whichever runs second reports a `400` naming the
problem rather than returning nothing. `buffered_body()` is the internal accessor for the
synchronous paths (`json`, `form`, `bind_json`), and returns a helpful error for a
streaming body.

Uploads stream through multer into `MultipartForm`. `collect_file` keeps the first
`max_upload_memory` bytes in memory and, past that, creates a `tempfile::TempPath` and
writes the rest there; `TempPath` removes the file on drop, so nothing has to clean up
after a failed or abandoned request. `sanitise_file_name` reduces a client-supplied name
to a safe basename — `copy_to_dir` is the only path helper, and it sanitises by
construction.

### The model layer (`model/`)

`Model<E>` is plain CRUD; `EntryModel<E>` adds primary-key shortcuts; `define_model!`
writes the newtype + `Deref` + `TableEntity` impl that Rust cannot express as embedding.
`TableEntity` gets `Table` for free via a blanket impl, which is what `ModelGroup` stores.

Conditions are SQL fragments with `?` placeholders — never string-interpolated values.
`Query::build` composes them through `Expr::cust_with_values`, which sea-query rewrites
per dialect. Column names in `order`/`group_by`/`where_eq` go through `is_plain_column` so
plain identifiers are quoted and anything else is passed through as an expression.

Counting is done by `count_rows`, which wraps the query in
`SELECT COUNT(*) FROM (…) AS sub_query`. sea-orm's `Paginator::count` requires
`E::Model: Sync`, a bound the generic layer would have to push onto every caller.

### `db::Database`

`Database` is a handle that is either a connection **pool** or an open **transaction**,
implementing sea-orm's `ConnectionTrait` so models work against both without changing
shape. `Database::transaction` hands the closure an owned handle; commit uses
`Arc::try_unwrap`, so a handle that leaked into a spawned task rolls back and reports why
rather than committing silently.

### Errors

One type, `web::Error`, carrying an HTTP code *and* a business code. Business codes
(1000+) are sent over HTTP 200 — `code::http_status` decides — so proxies do not mistake
them for transport failures. Plain `&str`/`String` become 500s.

## Conventions

These are enforced, not aspirational:

* **Idiomatic Rust naming.** No `get_` prefixes on getters (use the noun, or `as_`/`into_`
  for conversions); `has_*`/`is_*`/`contains_*` for predicates; `#[must_use]` on pure
  getters.
* **No Go-isms.** `Result<T, E = Error>` (not `FrameResult`), `Routes` (not `Handles`),
  `Paged<T>` (not `PageAble`), `MetaOption::value(..)` (not `with_value(..)`).
* **Standard traits.** Implement `Display`, `Debug`, `Default`, `From`, `Deref` where they
  mean something; manual `Debug` for types holding closures.
* **`unsafe` appears nowhere.** If you reach for it, restructure instead.
* **No panics in library code** outside genuine invariant violations (`expect` on a lock
  that cannot be poisoned in practice is fine; add a message saying why).
* **Document every public item.** Doc examples are compiled — `cargo test` runs them, and
  they must compile even when marked `no_run`.
* **Clippy stays clean** at `--all-targets`.
* Comments explain *why*, and only where the reason is not obvious from the code. Match
  the surrounding density; do not narrate.

## Deliberate non-goals (this iteration)

Captcha, QR codes, mail, image helpers, reverse proxy, the TLS listener, and the
`component/` crates of the Go original (`cache`, `ratelimit`, `cors`, `validator`,
`schedule`, `auth`). The extension points for them — `Filter`, `Service`, `Converter` —
are in place and exercised.

## Known gaps

* SSE streams use fixed headers — `SseStream` cannot add response headers, because the
  response is built before the handler runs.
* There is no chunked-upload helper for a client that streams *in* without a multipart
  envelope; such a body has to go through `read_body()`, which holds it in memory.
* TLS: `ServerConfig::ssl` is parsed and reported by `ssl_enabled()` but not yet wired to a
  listener.
