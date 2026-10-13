# rust-web-frame

**Declare policy on the route, not in the handler. Tag a route with `.with_meta(...)` — auth, permissions, rate limits — and one filter enforces it for every route that opted in. A generic ORM included.**

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

### 2. A generic model that erases CRUD boilerplate

Write the entity once; `define_model!` produces a typed model with every query method:

```rust
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "t_user")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub email: String,
    pub name: String,
    pub age: i32,
}

define_model!(UserModel, Entity, "t_user");
```

That is the whole model. Now use it anywhere:

```rust
let users = UserModel::new(db);

users.create_table().await?;                       // derives the schema from the entity
let user  = users.find_by_pk(1).await?;            // Option<user::Model>
let all   = users.find_all().await?;               // Vec<user::Model>
let adults = users.query()
    .where_("age >= ?", args![18])
    .order_by_desc("age")
    .list(50)
    .await?;
let total = users.query().where_("age >= ?", args![18]).count().await?;
let (rows, total) = users.page(&Page::new(1, 10)).await?;
let paged = users.page_for_web(&Page::new(1, 10)).await?;   // { total, list }

users.update().where_pk(1).set("name", "bob").exec().await?;
users.delete_by_pk(1).await?;
```

**String conditions, bound parameters.** `Where("age > ?", 18)` is written exactly as in the
Go original. `?` is understood by every backend — sea-query rewrites it to `$1` for
PostgreSQL — so there is no SQL injection surface and no per-dialect code.

**Aggregates.** `SUM`, `COUNT`, `AVG`, `GROUP BY`, `HAVING`:

```rust
#[derive(sea_orm::FromQueryResult)]
struct CityStat { city: String, total: Option<i64> }

let stats = orders.aggregate()
    .select("city, SUM(amount) AS total")
    .group_by("city")
    .having("SUM(amount) > ?", args![200])
    .rows::<CityStat>()
    .await?;

let sum: Option<f64> = orders.aggregate().select("SUM(amount)").value().await?;
```

**Transactions that keep the model API.** The closure receives a `Database` handle, so models
built from it participate in the transaction:

```rust
db.transaction(|tx| async move {
    let accounts = AccountModel::new(tx);
    accounts.update().where_pk(from).set("balance", lower).exec().await?;
    accounts.update().where_pk(to).set("balance", higher).exec().await?;
    Ok(())                       // Ok commits, Err rolls back
})
.await?;
```

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

let section: AppSection = config.unmarshal_key("app")?;
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
| **Uploads** | `MultipartForm`, `UploadedFile`, `sanitise_file_name`, streamed and spilling to temp files |
| **Extensibility** | `Filter`, `Converter`, `Runner`, `Rest`, `Service` |
| **Data** | `db::Database`, `db::transaction`, `Model<E>`, `EntryModel<E>`, `define_model!`, `ModelGroup`, `Transaction` |
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
| Embedding `*model.EntryModel[T, PK]` | `define_model!` → `Deref<Target = EntryModel<E>>` | The macro writes the embedding Rust cannot. |
| Reflection by struct name | `TypeId`-keyed registry, `ctx.service::<T>()` | No runtime reflection. |
| `ctx.Get(...).WithMeta(...)` returns `*Route` | `ctx.get(...)` returns `RouteHandle`, `with_meta` consumes it | Idiomatic Rust builder, same chaining. |
| `pageAble` | `Paged<T>` | — |
| `c.MultipartForm()` (parses into memory, spills past 32 MiB) | `req.multipart().await?` (streamed, spills past 1 MiB) | Streaming keeps a large upload off the heap entirely, not just under a threshold. |

Not implemented in this iteration: captcha, QR codes, mail, reverse proxy, the TLS
listener, and the component crates (`component/cache`, `component/ratelimit`, …). The
extension points for them — `Filter`, `Service`, `Converter` — are all in place.

---

## License

Apache-2.0.
