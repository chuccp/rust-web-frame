# rust-web-frame

**Declare policy on the route, not in the handler. Tag a route with `.with_meta(...)` — auth, permissions, rate limits — and one filter enforces it for every route that opted in.**

A Rust port of [go-web-frame](https://github.com/chuccp/go-web-frame)'s design, built on
[axum](https://docs.rs/axum) + [sea-orm](https://docs.rs/sea-orm) the way the original is
built on Gin + GORM.

**🌐 Language** · English • [中文](README_ZH.md)

---

## The three ideas

### 1. Declarative routing

Most policy is per-route, not global: `/login` must not require a token, `/admin/*` needs a
permission, `/upload` deserves a tighter rate limit. Instead of repeating that check in
every handler — or splitting controllers apart just to get a different middleware chain —
declare it on the route and enforce it once:

```rust
ctx.get("/api/login", handler(login)).with_meta(skip_auth());
ctx.get("/api/profile", handler(profile)).with_meta(require_auth());
ctx.post("/api/admin/users", handler(create_user))
   .with_meta(require_auth())
   .with_meta(require_permission("admin:create_user"));
```

The factories are ordinary functions returning `MetaOption`, kept next to the filter that
consumes them:

```rust
fn require_auth() -> MetaOption { MetaOption::value("require_auth", true) }
fn skip_auth() -> MetaOption { MetaOption::value("skip_auth", true) }
fn require_permission(permission: &str) -> MetaOption {
    MetaOption::value("require_permission", permission)
}
```

One filter serves every route. It asks each request whether *this* route opted in:

```rust
#[async_trait]
impl Filter for AuthFilter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        if !req.has_meta(require_auth()) || req.has_meta(skip_auth()) {
            return chain.next(req).await;             // the route did not opt in
        }
        let Some(token) = req.header("authorization") else {
            return Err(Error::unauthorized());
        };
        let claims = verify(token)?;
        req.set_attr("user", claims.name.clone());    // hand data to the handler

        if req.has_meta(require_permission("admin:create_user")) && !claims.is_admin {
            return Err(Error::forbidden().detail("admin:create_user is required"));
        }
        chain.next(req).await
    }
}
```

Protecting a new endpoint is one `.with_meta(...)` at its route, and a handler can never
forget the check. See [`examples/withmeta.rs`](examples/withmeta.rs) for the whole thing.

| Call | Where | Meaning |
|---|---|---|
| `route.with_meta(option)` | registration | Attach metadata. Chainable — call it repeatedly. |
| `MetaOption::value(key, value)` | registration | Set one key to a value. `has_meta` matches the exact value. |
| `MetaOption::key(["a", "b"])` | registration | Mark each key `true`. `has_meta` matches on **any**. |
| `req.has_meta(option)` | filter / handler | `true` if this route satisfies the option. |
| `req.handler_meta()` | filter / handler | The underlying `HandlerMeta` (`get`, `contains_key`, …). |
| `req.set_attr` / `req.attr::<T>` | filter → handler | Pass request-scoped data down the chain. |

### 2. Queries stay sea-orm's

The framework does not add a query language. It binds an entity to a connection, derives
the table from the entity, and puts it in the container — then gets out of the way:

```rust
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "t_user")]          // the table name lives here, and nowhere else
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub email: String,
    pub age: i32,
}

define_model!(
    /// Users of the shop.
    UserModel,
    Entity
);

impl UserModel {
    /// Business logic sits next to the entity it works on, written in sea-orm.
    pub async fn register(&self, email: &str, age: i32) -> Result<Model> {
        Ok(ActiveModel {
            email: Set(email.to_owned()),
            age: Set(age),
            ..Default::default()
        }
        .insert(self.db())
        .await?)
    }

    /// A transfer, atomically. `db()` is the pool outside the closure and the
    /// transaction inside it, so the same methods run either way.
    pub async fn transfer(&self, from: i64, to: i64, amount: i64) -> Result<()> {
        self.db()
            .clone()
            .transaction(|tx| {
                let accounts = self.with_db(tx);
                async move { /* sea-orm queries against accounts.db() */ Ok(()) }
            })
            .await
    }
}
```

And in a handler, it is sea-orm's API with a connection handed to it:

```rust
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

let users = req.context().service::<UserModel>()?;

// typed columns, checked by the compiler
let adults = user::Entity::find()
    .filter(user::Column::Age.gte(18))
    .order_by_desc(user::Column::Id)
    .all(users.db())
    .await?;

// an aggregate, written the way sea-orm writes them
let stats = user::Entity::find()
    .select_only()
    .column_as(user::Column::Id.count(), "users")
    .column_as(user::Column::Balance.sum(), "total")
    .into_model::<Stats>()
    .one(users.db())
    .await?;

// one page, in the framework's response envelope
let paged = users
    .page(user::Entity::find().filter(user::Column::Age.gte(18)), &page)
    .await?;
```

`filter(Column::Age.gte(18))` is checked at the call site, so a renamed column is a compile
error rather than a runtime one — which a string condition like `"age > ?"` can never be.
Everything sea-orm documents applies as written: `find_with_related`, `paginate`,
`update_many`, `from_raw_sql`, `DerivePartialModel`, migrations, `sea-orm-cli`-generated
entities.

What the layer adds is exactly three things sea-orm cannot know about:

| | |
|---|---|
| **A connection that is a pool *or* a transaction** | `Database` is a handle, so `model.db()` works on both sides of `transaction()` without changing shape. |
| **Schema from the entity** | `model.create_table()` asks sea-orm's `Schema` to derive `CREATE TABLE` from the entity. |
| **A place in the container** | `builder.model(...)` registers it, `ctx.service::<UserModel>()` retrieves it, and the table is created at startup. |

### 3. One lifecycle

`Builder` collects components; `WebFrame` initialises them through a dependency-injection
`Context` and serves them.

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);

    builder.service(UserService::default());   // dependencies first
    builder.rest(UserController);              // controllers look them up in `init`
    builder.filter(AuthFilter);
    builder.runner(CleanupTask);
    builder.model(UserModel::new(db));

    builder.build().run().await?;
    Ok(())
}
```

Components are initialised in registration order and a component can only look up the ones
registered before it — register dependencies first. A component that also plays a second
role (a filter that a handler reads back) registers **once** and is found by its own type.

---

## Quick start

```bash
git clone https://github.com/chuccp/rust-web-frame   # not yet on crates.io
cd rust-web-frame
cargo run --example helloworld    # http://localhost:19009/
cargo run --example rest          # a controller + service + background runner
cargo run --example model         # entities, queries, transactions (in-memory SQLite)
cargo run --example withmeta      # declarative routing
cargo run --example upload        # streamed multipart uploads (port 19012)
```

```rust
use rust_web_frame as wf;
use wf::config::load_auto_config;
use wf::web::handler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);
    builder.get("/", handler(|_req| async { Ok("hello world") }));
    builder.build().run().await?;
    Ok(())
}
```

---

## Configuration

Config files are searched in the same places the Go original looks, later ones winning:

1. `/etc/<app>/config.{json,yaml,yml,toml}`
2. `~/.<app>/config.{json,yaml,yml,toml}`
3. `./config/…`
4. `./config.…`

```yaml
web:
  server:
    port: 8080
    contextPath: /api          # prefixes every route
    locations: ["./public"]    # static files for unmatched requests
    page404: index.html        # SPA fallback
    maxBodySize: 10485760
  restGroups:                  # move a group to another port without a rebuild
    admin:
      port: 8081

db:
  type: sqlite                 # sqlite | mysql | postgres
  path: ./data.db

log:
  level: info                  # a tracing filter directive
  path: ./logs/app.log         # optional, rotates daily
  format: text                 # text | json
```

MySQL and PostgreSQL take `host`/`port`/`user`/`password`/`database`. Anything else sea-orm
can reach — ClickHouse, SQL Server — works through `db.url` with no framework support.

```rust
let config = load_auto_config()?;
let port: u16 = config.get_or("web.server.port", 19009);
assert!(config.contains_key("web.server"));

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AppSection { greeting: String }

let section: AppSection = config.section("app")?;
```

---

## File uploads

A `multipart/form-data` body is **streamed, never buffered whole**. Each file is held in
memory up to `web.server.maxUploadMemory` (1 MiB by default) and spills to a temporary
file past that — so a 2 GB upload costs disk, not RAM, and the temp file is deleted as
soon as the request is done with it.

```rust
async fn upload(req: Request) -> Result<Message> {
    let form = req.multipart().await?;

    let caption = form.text_or("caption", "(untitled)").to_owned();
    let photo = form
        .file("photo")
        .ok_or_else(|| Error::bad_request().detail("a `photo` file is required"))?;

    // `copy_to_dir` sanitises the client-supplied name, so `../../etc/passwd`
    // lands as `passwd` inside the upload directory.
    let saved = photo.copy_to_dir("./uploads").await?;

    Ok(Message::data(json!({
        "caption": caption,
        "fileName": photo.sanitised_file_name(),
        "contentType": photo.content_type(),
        "size": photo.size(),
        "spilledToDisk": !photo.is_in_memory(),
        "savedAs": saved.display().to_string(),
    })))
}
```

`MultipartForm` gives you both flavours of every accessor, because a name may repeat
(multi-file inputs, checkbox groups):

| Call | Meaning |
|---|---|
| `form.text(name)` / `text_or(name, default)` | first value of a text field |
| `form.texts(name)` | every value of a repeated text field |
| `form.file(name)` / `form.files(name)` | first / every file under a field name |
| `form.all_files()` / `form.file_count()` | every file in the form |

`UploadedFile` keeps the content wherever it landed behind one API:

| Call | Meaning |
|---|---|
| `file.file_name()` | the name the client sent — **never** use it as a path |
| `file.sanitised_file_name()` | that name reduced to a safe basename |
| `file.content_type()` / `file.size()` | declared type, byte count |
| `file.bytes().await?` | the whole content, from memory or from the temp file |
| `file.copy_to(path).await?` | writes it out, creating parent directories |
| `file.copy_to_dir(dir).await?` | writes it under the *sanitised* name, returns the path |
| `file.temp_path()` | the spill file, when there is one |

`Request::save_uploaded_file(file, destination)` is the Go original's `SaveUploadedFile`.

### Limits

| Setting | Default | Meaning |
|---|---|---|
| `web.server.maxBodySize` | 10 MiB (`-1` = unlimited) | whole request body; over it is `413` |
| `web.server.maxUploadMemory` | 1 MiB | per-file in-memory budget; past it spills to a temp file |

### Server limits (all of them live)

| Setting | Default | Meaning |
|---|---|---|
| `readHeaderTimeout` | 15 s | time a client may take over its request headers; one that dribbles them is disconnected |
| `readTimeout` | none | time a client may take over the request **body**; over it is `408` and the connection closes |
| `maxHeaderBytes` | 16 KiB | ceiling on the request head. Values below 8 KiB are raised to it — that is the smallest read buffer an HTTP/1 connection can use |

`readTimeout` bounds *reading* only. A slow **response** — an SSE stream, a WebSocket — is
never cut off by it.

### CORS

Omitted, no CORS headers are sent at all, which is the safe default: a browser on another
origin simply cannot read the response. Configured, the policy is applied outside the
router, so a preflight `OPTIONS` is answered even though it matches no route — without it
that request would be a `405`:

```yaml
web:
  server:
    cors:
      allowedOrigins: ["https://app.example.com"]   # or ["*"]
      allowedMethods: [GET, POST]
      allowedHeaders: [content-type, authorization]
      allowCredentials: true
      maxAge: 3600
```

An origin or method that is not a valid header is a startup error, not a request that
quietly loses a header.

### `contextPath`, and where static files live

`contextPath` prefixes every route. Written as `api`, `/api/` or `/api` it means the same
thing — it is normalised to `/api` — and the prefix is stripped again when the fallback
looks a request up in `locations`, so an SPA can sit at `/` while the API sits at `/api`.

`locations` map onto the URL **root**: each directory *is* the root, so
`./public/report.csv` is served at `/report.csv`, not `/public/report.csv`.

### Static files

`routes.static_files("/assets", "./public")` and the `web.server.locations` setting both
stream from disk and speak the full HTTP file protocol — `ETag`, `Last-Modified`,
`If-None-Match`, `If-Modified-Since`, `Range` with `206`/`416`, `Accept-Ranges`, directory
indexes, and `HEAD` without a body. Requests are canonicalised before the containment
check, so `..` and symlinks cannot escape the root.

### Observability

Every request logs one line at `debug` — with the **route template**, so `/users/:id` is
what you see, not `/users/42`:

```
DEBUG rust_web_frame::web::server: request method=GET route="/users/:id" path="/users/42" status=200 elapsed_ms=1 client=127.0.0.1
```

Turn it on with `log.level: debug` or a narrow `rust_web_frame=debug`. Failures are logged
at `error` whether or not access logging is on.

A handler that panics is caught: the client gets the standard `500` envelope (never the
panic message), the server keeps serving, and the message goes to the log at `error`.

Two rules worth knowing: a body can be read **once** — calling `multipart()` and then
`read_body()` fails with a clear `400` rather than silently returning nothing — and
`req.page()` on a multipart request reads `pageNo`/`pageSize` from the query string,
since the body holds files rather than JSON.

---

## What is in the box

| Area | Types |
|---|---|
| **Routing** | `Context::get/post/put/delete/patch/any`, `Routes`, `RouteHandle`, Gin-style `:id` and `*path` templates, context paths, `static_files`, 404/405 envelopes |
| **Requests** | `Request` — params, query, form, JSON binding, multipart uploads, pagination, headers, cookies, client IP, attributes, `has_meta` |
| **Responses** | `Response`, `Message` (`{code, data, msg, type}`), `FileResponse`, redirects, raw bodies, `IntoResponse` for the types handlers actually return |
| **Errors** | `Error` with HTTP *and* business codes, `?`-friendly conversions from `io::Error`, `sea_orm::DbErr`, `serde_json::Error`, `String`, `&str` |
| **Streaming** | `SseStream`, `WebSocketStream` |
| **Ops** | access log (one line per request, at `debug`), panic isolation with the standard 500 envelope, request timeouts, CORS, graceful shutdown with bounded runner and connection graces |
| **Uploads** | `MultipartForm`, `UploadedFile`, `sanitise_file_name`, streamed and spilling to temp files |
| **Extensibility** | `Filter`, `Converter`, `Runner`, `Rest`, `Service` |
| **Data** | `db::Database` (pool *or* transaction), `db::transaction`, `Model<E>`, `define_model!`, `ModelGroup` — queries themselves are [sea-orm](https://docs.rs/sea-orm)'s |
| **Pagination** | `Page`, `Paged<T>` |
| **Components** | `Service`, `Rest`, `Filter`, `Runner`, `Converter`, `Context`, `RestGroup`, `Runtime`, `Builder` |

### Handler return types

A handler declares its own return type; anything implementing `IntoResponse` works:

```rust
async fn text(_req: Request) -> Result<&'static str> { Ok("plain text") }
async fn json(_req: Request) -> Result<Response>    { Ok(Response::json(json!({"ok": true}))) }
async fn envelope(_req: Request) -> Result<Message> { Ok(Message::data(json!({"id": 1}))) }
async fn typed(req: Request) -> Result<Message> {
    let payload: CreateUser = req.bind_json()?;
    Ok(Message::data(json!(payload)))
}
async fn missing(_req: Request) -> Result<Response> { Err(Error::not_found().detail("no such user")) }
```

Handlers share state by capturing an `Arc` of a registered component, or by looking it up
per request:

```rust
fn service(req: &Request) -> Arc<UserService> {
    req.context().service::<UserService>().expect("registered at startup")
}
```

### Error mapping

| Situation | HTTP | Envelope `code` |
|---|---|---|
| `Ok(...)` | 200 | 200 |
| `Err(Error::not_found())` | 404 | 404 |
| `Err(Error::forbidden())` | 403 | 403 |
| `Err(Error::token_expired())` | **200** | 1003 |
| `Err("boom".into())` | 500 | 500 |
| no route matched | 404 | 404 |
| path matched, method did not | 405 | 405 |

Business codes (1000+) travel over HTTP 200 so browsers and proxies do not mistake a
domain-level failure for a transport-level one; the envelope's `code` carries the meaning.

### Serving several ports

```rust
let admin = RestGroup::builder("admin")
    .server_config(ServerConfig::on_port(8081).with_context_path("/admin"))
    .routes(|ctx| { ctx.get("/stats", handler(stats)); })
    .build();

builder.rest_group(admin);
```

---

## Development

```bash
cargo test                      # unit, integration and doc tests
cargo clippy --all-targets      # no warnings
cargo fmt
cargo run --example rest        # run an example
cargo test --features mysql     # build against another driver
```

Feature flags: `sqlite` (default), `mysql`, `postgres`.

---

## Differences from the Go original

The design is the same; the language forces a few honest changes.

| go-web-frame | rust-web-frame | Why |
|---|---|---|
| `return "hello", nil` | `Ok("hello")` with a declared return type | Rust has no `any`; `IntoResponse` covers the same ground without losing the type. |
| `errors.New("boom")` | `Err("boom".into())` | One error type, `Error`, with `From` impls for the usual sources. |
| `c.Query("page")` | `req.query("page") -> Option<&str>` | Absence is in the type; `query_or`, `query_i64` cover the ergonomic cases. |
| `Query.One()` returns a zero value + nil error | `query().one() -> Result<Option<T>>` | The type answers "did I find anything?". `get_by_pk` returns `404` instead. |
| `Model.Save(&user)` (upsert) | `insert(am)`, `update_by_pk(am)` | Two operations, two methods; no hidden branch on whether the key is set. |
| A `Model[T]` / `EntryModel[T, PK]` wrapper *around* GORM | `Model<E>` bound *to* sea-orm | The Go layer exists to give GORM generics and a PK-aware API. sea-orm already has both, so wrapping it again would only hide it. |
| `Query.Where("age > ?", 18)` | `filter(Column::Age.eq(18))` | Typed columns are checked at the call site; a string condition can only fail at runtime. |
| Embedding `*model.EntryModel[T, PK]` | `define_model!` → `Deref<Target = Model<E>>` | The macro writes the embedding Rust cannot. |
| `Viper.UnmarshalKey` | `config.section::<T>("web.server")` | Deserialising a config section is not "unmarshalling a key". |
| `Message{code, data, msg, type}` | `Message{code, data, message, kind}` (serde renames keep the wire format) | `type` is a keyword, and `msg` is an abbreviation. |
| `PageAble.List` | `Paged.items` (serialised as `list`) | — |
| `gin.HandlerFunc` | `web::Handler` | Named after what it is, not after the package that introduced it. |
| Reflection by struct name | `TypeId`-keyed registry, `ctx.service::<T>()` | No runtime reflection. |
| `ctx.Get(...).WithMeta(...)` returns `*Route` | `ctx.get(...)` returns `RouteHandle`, `with_meta` consumes it | Idiomatic Rust builder, same chaining. |
| `c.MultipartForm()` (parses into memory, spills past 32 MiB) | `req.multipart().await?` (streamed, spills past 1 MiB) | Streaming keeps a large upload off the heap entirely, not just under a threshold. |

Not implemented in this iteration: captcha, QR codes, mail, reverse proxy, the TLS
listener, and the component crates (`component/cache`, `component/ratelimit`, …). The
extension points for them — `Filter`, `Service`, `Converter` — are all in place.

---

## License

Apache-2.0.
