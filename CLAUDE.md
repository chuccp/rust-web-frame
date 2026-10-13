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

## Releasing

Push a `vX.Y.Z` tag, and `.github/workflows/release.yml` does the rest: tests, clippy and
`cargo fmt --check` on what was tagged; the tag's version written into `Cargo.toml` and
`Cargo.lock` and committed to `main` as `chore(release): vX.Y.Z`; the GitHub Release built
from the tag's own message; `cargo publish --dry-run`; and `cargo publish` when
`CARGO_REGISTRY_TOKEN` is set. The tag is therefore the one place a version is written by
hand, and the commit it points at is the one *before* the bump — the version on crates.io is
what the tag names.

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
    mod.rs          Model<E>, TableEntity/Table, define_model!, has_table
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

Three invariants that are easy to break:

* **`init` must take `&self`, not `&mut self`.** Shared receivers are what let one
  instance be an `Arc` from the start, serve as a service *and* as a filter, and be
  initialised without exclusive access. Components that need startup state use
  `OnceLock`.
* **The registration key must be the user's type**, never an adapter's. Registering a
  filter as `AsFilter<Counter>` would make `ctx.service::<Counter>()` fail; there are
  regression tests for exactly this in `core/service.rs` and `tests/web_layer.rs`.
* **A component configures itself in place.** `init_all` registers the instance the caller
  built, and refuses a type it already holds, so an `init` that registered *another* copy
  of its own type fails at startup with "registered twice". Settings read from the config
  therefore go behind a lock or a `OnceLock` — `RateLimit`, `Captcha` and `Proxy` do that —
  or the caller builds the component from the config, as `CacheService::from_config` and
  `LocalCache::from_config` do.

A component is only visible to components registered *after* it. Document that; do not
try to make lookups order-independent.

### Routing (`web/route.rs`, `web/server.rs`)

Routes are keyed by `(path, method)`. Paths are written in Gin syntax (`/users/:id`,
`/static/*filepath`) and translated by `to_axum_path` for matchit; `/{id}` also passes
through. `RouteHandle` wraps `Arc<Route>` and `with_meta` consumes it — idiomatic builder,
same chaining as `ctx.Get(...).WithMeta(...)` in the Go original.

`Server::validate` runs before binding and turns three mistakes into startup errors instead
of panics or silence: duplicate `(path, method)` pairs, path templates matchit rejects, and
a route under a `Routes::require_meta` prefix that does not carry the metadata the prefix
requires. It probes each *distinct* path once, because several routes legitimately share one
path; the guard check is what answers "did every route that needs this policy ask for it?",
which nothing else can, because metadata is opt-in by design — a route that forgets it asks
for nothing and would otherwise serve unprotected. Guards travel with the routes they were
declared beside, so `Server::merge_routes` carries them into the default server.

Dispatch (`Server::router` + `dispatch`): read the body under `effective_max_body_size`,
extract path params and a pending `WebSocketUpgrade`, build a `Request`, run the
`FilterChain`, hand the result to the `Converter`, and log one access line. Requests that
match no route go to `fallback` (static files → SPA page → 404 envelope); requests whose
path matched but method did not go to the `MethodRouter` fallback and get a 405 envelope.

Two layers sit outside the chain, applied last so they cover the fallback too:

* `CatchPanicLayer` with [`PanicHandler`](src/web/server.rs). A panic unwinds into hyper
  otherwise: the client sees a connection reset or a bare 503 with no envelope, and the
  only trace is the default panic hook, which bypasses `tracing` and never reaches the
  log file. The panic message goes to the log at `error`; the client gets a generic 500.
* The access log. One line per request at `debug`, carrying the **route template** rather
  than just the path, so `/users/:id` is what the log shows. `log.level: debug` (or
  `rust_web_frame=debug`) turns it on; failures are logged at `error` regardless. A
  request that panics has no access line — the panic handler logs instead, and the layer
  has no route context.

Static files go through `tower_http::services::ServeFile`, which streams from disk and
implements `Range`, `ETag`, `Last-Modified`, the conditional headers, `Accept-Ranges` and
the `206`/`304`/`416` responses that go with them. `web::routes::serve_file` adds only what
`ServeFile` cannot do: keep the request inside the root (canonicalise, then compare — so
`..` and symlinks are already resolved) and report a miss as the envelope.

### The listener (`web/listener.rs`)

The accept loop is written out rather than delegated to `axum::serve`, which builds its
hyper connection with stock defaults and exposes no way in — with it, `read_header_timeout`
and `max_header_bytes` were configuration that changed nothing. What this buys:

* `header_read_timeout` and `max_buf_size` on the http/1 builder. hyper *asserts* a
  minimum buffer of 8 KiB, so `ServerConfig::effective_max_header_bytes` raises anything
  smaller with a warning rather than letting a connection panic later.
* `read_timeout` has no hyper equivalent (it covers the body, not the head), so it is
  applied where the body is read: `request::within_read_timeout` wraps both `read_to_bytes`
  in dispatch and the multipart parse. It bounds *reading* only, so a slow response — SSE,
  WebSocket — is untouched. A `408` closes the connection, because a client that never
  finished its body would otherwise have its next bytes parsed as this request's tail.
* Connection draining, with the Go original's 30 s `DRAIN_GRACE`.

Everything else follows axum's own shape: a task per connection,
`serve_connection_with_upgrades` so WebSockets still upgrade, and a `WithPeer` service that
inserts `ConnectInfo` — axum's `into_make_service_with_connect_info` resolves through the
`Connected` trait, which is ambiguous for a bare `SocketAddr`.

### TLS (`web/tls.rs`)

`CertStore` answers one question — which certificate for this SNI name — in a fixed order:
exact local match, wildcard, **ACME**, first local certificate, self-signed. The order is
the point. The Go original had the wildcard matcher end in `generateSelfSigned`, so it never
returned nil and the `autocert.GetCertificate` call below it was unreachable: no
certificate was ever ordered, and every unknown name got a self-signed one that was then
written into the exact-match map and stuck. Do not "simplify" the order back.

Two constraints worth knowing before touching this file:

* `ResolvesServerCert::resolve` runs on the handshake path and is **synchronous**. It must
  not read files. Local certificates are therefore loaded at startup and re-read by
  `watch_files`, a background task that checks mtimes every `RELOAD_INTERVAL`; the resolver
  only ever touches memory.
* `rustls::server::ClientHello` has private fields and no constructor, so the ACME resolver
  has to be handed the *real* hello — which is why the ordering lives inside `resolve`
  rather than in a `lookup(name)` helper.

ACME uses TLS-ALPN-01 (`UseChallenge::TlsAlpn01`), so the challenge certificate is served
by the same resolver and no port 80 listener is needed. `server_config` puts `acme-tls/1`
first in the ALPN list for the same reason.

The crypto provider is pinned to `ring` in Cargo.toml (`rustls`, `tokio-rustls` and
`rustls-acme` all use it). Installing two providers would make
`CryptoProvider::install_default` ambiguous at runtime.

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

**Queries are sea-orm's. Do not add a query language on top of it.** `Model<E>` holds a
`Database` and offers only what sea-orm's entity API cannot express on its own:

* `new` / `db` / `with_db` — the handle, and the same handle bound to a transaction.
* `table_name` — from the entity's `#[sea_orm(table_name = "…")]`. The table name is
  deliberately *not* a constructor argument: keeping it in two places is how they drift.
* `create_table` / `drop_table` — schema derived from the entity by sea-orm's `Schema`.
* `page(select, &Page)` / `count(select)` — a sea-orm `Select` in, the framework's
  `Paged<T>` out. Both take sea-orm's type, so filters compose the normal way.

There is no `Query`/`Update`/`Delete`/`Aggregate` builder, and adding one back is the thing
to avoid: an earlier version had all four (~700 lines) and they were strictly worse than
what sea-orm already ships — string conditions that fail at runtime instead of compile
time, string column names, and a `Query::execute(sql)` that appended `AND …` without a
`WHERE`. If something seems missing, check whether sea-orm has it first; it usually does.

`define_model!` writes the newtype + `Deref` + `TableEntity` impl that Rust cannot express
as embedding. `TableEntity` (entity-facing: `type Entity`, `model()`) gets `Table` (the
object-safe trait `ModelGroup` stores) for free via a blanket impl — the split exists
because `dyn Table` cannot name an unspecified associated type.

`Model::page`/`count` need `where E::Model: Send + Sync`, because sea-orm's
`PaginatorTrait` is implemented for `Select<E>` only when the entity's model is `Sync`.
That bound is on the two methods rather than on `impl<E: EntityTrait> Model<E>`, so it does
not leak into `create_table` and friends.

### `db::Database`

`Database` is a handle that is either a connection **pool** or an open **transaction**,
implementing sea-orm's `ConnectionTrait` so models work against both without changing
shape. `Database::transaction` hands the closure an owned handle; commit uses
`Arc::try_unwrap`, so a handle that leaked into a spawned task rolls back and reports why
rather than committing silently.

`DbConfig` — the `db` section — is sea-orm's `ConnectOptions` written in YAML: a `url` (the
connection string sqlx and `DATABASE_URL` take, so the scheme picks the driver) and one key
per setter, flat, `None` meaning "sea-orm's own default". The Go original's `type`, `host`,
`port`, `user`, `password` and `database` fields were dropped deliberately: assembling a url
from them means two sources of truth for where the database is, and a `type` that can
disagree with the scheme. Do not add them back. `DbConfig::sqlite(path)` builds a
`sqlite://…?mode=rwc` url, since sqlx will not create a file without it; a url from a config
file is passed through untouched.

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

## Deliberate non-goals

The Go original's `util` image helpers, and `cors` as a component — CORS is a property of
the listener here (`web.server.cors`, applied outside the router and therefore covering the
fallback too), not of a route, and a component would only move it back inside.

The rest of the Go original's `component/` is ported: `auth`, `cache`, `captcha`,
`localcache`, `mail`, `proxy`, `qrcode`, `ratelimit`, `schedule`, `validator`. Keep them
ordinary: each is a `Service` or a `Filter` on the extension points the framework already
has, and nothing in `src/component/` may need a change to the framework to work.

## Known gaps

* The ACME path is exercised up to the point of a real authority: everything except
  talking to Let's Encrypt is tested (resolver ordering, challenge handling, cache
  directory), but no test can complete an issuance without a domain and an account.
* SSE streams use fixed headers — `SseStream` cannot add response headers, because the
  response is built before the handler runs.
* There is no chunked-upload helper for a client that streams *in* without a multipart
  envelope; such a body has to go through `read_body()`, which holds it in memory.
* Shutdown waits a bounded grace for runners (5 s) and in-flight connections (30 s), then
  abandons them. A runner that ignores its cancellation token has its work stopped at an
  arbitrary point.
* The reverse proxy forwards a request body the framework has already read, and does not
  tunnel WebSocket upgrades — the server gives an upgrade to the handler that asked for it.
  Its `https` upstreams are trusted through `webpki`'s roots, not the operating system's.
