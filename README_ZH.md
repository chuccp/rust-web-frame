# rust-web-frame

**把策略声明在路由上，而不是散落在 handler 里。用 `.with_meta(...)` 给路由打标签——鉴权、权限、限流——一个 filter 为所有"声明过"的路由统一执法。**

这是 [go-web-frame](https://github.com/chuccp/go-web-frame) 设计思路的 Rust 版本，与原版
「建在 Gin + GORM 之上」一样，它建在 [axum](https://docs.rs/axum) + [sea-orm](https://docs.rs/sea-orm) 之上。

**🌐 语言** · [English](README.md) • 中文

---

## 三个核心想法

### 1. 声明式路由

大部分策略是**按路由**的，不是全局的：`/login` 不该要 token，`/admin/*` 需要权限，
`/upload` 值得更严的限流。不必在每个 handler 里重复这些检查，也不必为了换一条中间件链
而把 controller 拆开——在路由上声明一次，在一个 filter 里统一执法：

```rust
ctx.get("/api/login", handler(login)).with_meta(skip_auth());
ctx.get("/api/profile", handler(profile)).with_meta(require_auth());
ctx.post("/api/admin/users", handler(create_user))
   .with_meta(require_auth())
   .with_meta(require_permission("admin:create_user"));
```

这些工厂就是普通函数，返回 `MetaOption`，和消费它的 filter 放在一起：

```rust
fn require_auth() -> MetaOption { MetaOption::value("require_auth", true) }
fn skip_auth() -> MetaOption { MetaOption::value("skip_auth", true) }
fn require_permission(permission: &str) -> MetaOption {
    MetaOption::value("require_permission", permission)
}
```

一个 filter 服务所有路由。它逐个请求地问：**这条**路由声明了吗？

```rust
#[async_trait]
impl Filter for AuthFilter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        if !req.has_meta(require_auth()) || req.has_meta(skip_auth()) {
            return chain.next(req).await;             // 这条路由没有 opt in
        }
        let Some(token) = req.header("authorization") else {
            return Err(Error::unauthorized());
        };
        let claims = verify(token)?;
        req.set_attr("user", claims.name.clone());    // 把数据交给 handler

        if req.has_meta(require_permission("admin:create_user")) && !claims.is_admin {
            return Err(Error::forbidden().detail("admin:create_user is required"));
        }
        chain.next(req).await
    }
}
```

保护一个新接口，就是在它的路由上加一个 `.with_meta(...)`；handler 永远没法"忘了检查"。
完整示例见 [`examples/withmeta.rs`](examples/withmeta.rs)。

| 调用 | 位置 | 含义 |
|---|---|---|
| `route.with_meta(option)` | 注册时 | 附加元数据。可链式调用，可多次调用。 |
| `MetaOption::value(key, value)` | 注册时 | 设置一个键值。`has_meta` 精确匹配值。 |
| `MetaOption::key(["a", "b"])` | 注册时 | 把每个键设为 `true`。`has_meta` **任一**命中即匹配。 |
| `req.has_meta(option)` | filter / handler | 当前路由是否满足该 option。 |
| `req.handler_meta()` | filter / handler | 底层的 `HandlerMeta`（`get`、`contains_key` 等）。 |
| `req.set_attr` / `req.attr::<T>` | filter → handler | 沿链条向下传递请求级数据。 |

### 2. 查询就是 sea-orm

框架不另造一套查询语言。它把实体绑到连接上、从实体推导表名、把它放进容器——然后让开：

```rust
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "t_user")]          // 表名只在这里，不在别处
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub email: String,
    pub age: i32,
}

define_model!(
    /// 商城的用户。
    UserModel,
    Entity
);

impl UserModel {
    /// 业务逻辑就写在它操作的那个实体旁边，用 sea-orm 写。
    pub async fn register(&self, email: &str, age: i32) -> Result<Model> {
        Ok(ActiveModel {
            email: Set(email.to_owned()),
            age: Set(age),
            ..Default::default()
        }
        .insert(self.db())
        .await?)
    }

    /// 原子转账。`db()` 在闭包外是连接池、闭包内是事务，同一套方法两边都能跑。
    pub async fn transfer(&self, from: i64, to: i64, amount: i64) -> Result<()> {
        self.db()
            .clone()
            .transaction(|tx| {
                let accounts = self.with_db(tx);
                async move { /* 用 accounts.db() 跑 sea-orm 查询 */ Ok(()) }
            })
            .await
    }
}
```

在 handler 里就是把连接交给 sea-orm 的原生 API：

```rust
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

let users = req.context().service::<UserModel>()?;

// 类型化的列，编译期检查
let adults = user::Entity::find()
    .filter(user::Column::Age.gte(18))
    .order_by_desc(user::Column::Id)
    .all(users.db())
    .await?;

// 聚合，就按 sea-orm 的写法
let stats = user::Entity::find()
    .select_only()
    .column_as(user::Column::Id.count(), "users")
    .column_as(user::Column::Balance.sum(), "total")
    .into_model::<Stats>()
    .one(users.db())
    .await?;

// 一页数据，装进框架的响应信封
let paged = users
    .page(user::Entity::find().filter(user::Column::Age.gte(18)), &page)
    .await?;
```

`filter(Column::Age.gte(18))` 在调用处就被检查——改个列名是**编译错误**而不是运行时错误，这是
`"age > ?"` 这种字符串条件永远做不到的。sea-orm 文档里写的一切原样适用：`find_with_related`、
`paginate`、`update_many`、`from_raw_sql`、`DerivePartialModel`、迁移、`sea-orm-cli` 生成的实体。

这一层只加了三件 sea-orm 不可能知道的事：

| | |
|---|---|
| **既是连接池又是事务的连接** | `Database` 是句柄，所以 `model.db()` 在 `transaction()` 内外形状一致。 |
| **从实体建表** | `model.create_table()` 让 sea-orm 的 `Schema` 从实体推导 `CREATE TABLE`。 |
| **在容器里的位置** | `builder.model(...)` 注册，`ctx.service::<UserModel>()` 取出，启动时自动建表。 |

### 3. 一套生命周期

`Builder` 收集组件；`WebFrame` 通过依赖注入的 `Context` 初始化它们并对外服务。

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);

    builder.service(UserService::default());   // 依赖先注册
    builder.rest(UserController);              // controller 在 init 里把它们取出来
    builder.filter(AuthFilter);
    builder.runner(CleanupTask);
    builder.model(UserModel::new(db));

    builder.build().run().await?;
    Ok(())
}
```

组件按注册顺序初始化，一个组件只能查到**在它之前**注册的组件——所以先注册依赖。一个组件
如果还扮演第二个角色（比如一个 filter，handler 还要把它读回来），**只注册一次**，之后按
它自己的类型就能查到。

---

## 快速开始

```bash
cargo add --git https://github.com/chuccp/rust-web-frame   # 尚未发布到 crates.io
cargo run --example helloworld    # http://localhost:19009/
cargo run --example rest          # controller + service + 后台任务
cargo run --example model         # 实体、查询、事务（内存 SQLite）
cargo run --example withmeta      # 声明式路由
cargo run --example upload        # 流式 multipart 上传（端口 19012）
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

## 配置

配置文件搜索路径与 Go 原版一致，越靠后优先级越高：

1. `/etc/<app>/config.{json,yaml,yml,toml}`
2. `~/.<app>/config.{json,yaml,yml,toml}`
3. `./config/…`
4. `./config.…`

```yaml
web:
  server:
    port: 8080
    contextPath: /api          # 给所有路由加前缀
    locations: ["./public"]    # 未匹配请求的静态文件目录
    page404: index.html        # SPA 回退页
    maxBodySize: 10485760
  restGroups:                  # 不改代码就能把某个组挪到另一个端口
    admin:
      port: 8081

db:
  type: sqlite                 # sqlite | mysql | postgres
  path: ./data.db

log:
  level: info                  # tracing 过滤指令
  path: ./logs/app.log         # 可选，按天滚动
  format: text                 # text | json
```

MySQL / PostgreSQL 用 `host`/`port`/`user`/`password`/`database`。其它 sea-orm 能连的库
（ClickHouse、SQL Server）通过 `db.url` 直接支持，框架不需要为它们写代码。

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

## 文件上传

`multipart/form-data` 的请求体是**流式处理、绝不整体缓冲**的。每个文件在内存里最多留
`web.server.maxUploadMemory`（默认 1 MiB），超过就落到临时文件——所以一个 2 GB 的上传消耗的
是磁盘而不是内存，请求用完即删。

```rust
async fn upload(req: Request) -> Result<Message> {
    let form = req.multipart().await?;

    let caption = form.text_or("caption", "(untitled)").to_owned();
    let photo = form
        .file("photo")
        .ok_or_else(|| Error::bad_request().detail("a `photo` file is required"))?;

    // `copy_to_dir` 会先把客户端给的文件名消毒，所以 `../../etc/passwd`
    // 落到上传目录里是 `passwd`。
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

字段名可能重复（多文件输入框、复选框组），所以 `MultipartForm` 每个访问器都有"取第一个"和
"取全部"两种：

| 调用 | 含义 |
|---|---|
| `form.text(name)` / `text_or(name, default)` | 文本字段的第一个值 |
| `form.texts(name)` | 重复文本字段的全部值 |
| `form.file(name)` / `form.files(name)` | 某字段名下第一个 / 全部文件 |
| `form.all_files()` / `form.file_count()` | 表单里的全部文件 |

`UploadedFile` 把"内容到底在内存还是磁盘"藏在一套 API 后面：

| 调用 | 含义 |
|---|---|
| `file.file_name()` | 客户端发来的名字——**绝不能**直接当路径用 |
| `file.sanitised_file_name()` | 上面这个名字消毒后的安全 basename |
| `file.content_type()` / `file.size()` | 声明的类型、字节数 |
| `file.bytes().await?` | 全部内容，内存或临时文件都能读 |
| `file.copy_to(path).await?` | 写出到指定路径，自动建父目录 |
| `file.copy_to_dir(dir).await?` | 用**消毒后**的文件名写入，返回落盘路径 |
| `file.temp_path()` | 落到磁盘时的临时文件路径 |

`Request::save_uploaded_file(file, destination)` 对应 Go 原版的 `SaveUploadedFile`。

### 限制

| 配置 | 默认值 | 含义 |
|---|---|---|
| `web.server.maxBodySize` | 10 MiB（`-1` 为不限） | 整个请求体；超出返回 `413` |
| `web.server.maxUploadMemory` | 1 MiB | 单文件内存预算；超出落临时文件 |

### 服务器限制（全部生效）

| 配置 | 默认值 | 含义 |
|---|---|---|
| `readHeaderTimeout` | 15 秒 | 客户端发请求头的最长时间；一个字节一个字节磨的客户端会被断开 |
| `readTimeout` | 不限 | 客户端发请求**体**的最长时间；超出返回 `408` 并关闭连接 |
| `maxHeaderBytes` | 16 KiB | 请求头上限。小于 8 KiB 会被抬到 8 KiB——那是 HTTP/1 连接能用的最小读缓冲 |

`readTimeout` 只约束**读取**。慢**响应**——SSE 流、WebSocket——永远不会被它掐断。

### CORS

不配置就完全不发 CORS 头，这是安全默认：其它源的浏览器读不到响应。配置了就会在路由之外套一层，
所以预检 `OPTIONS` 能得到应答——没有这一层时，那个请求会是个 `405`：

```yaml
web:
  server:
    cors:
      allowedOrigins: ["https://app.example.com"]   # 或 ["*"]
      allowedMethods: [GET, POST]
      allowedHeaders: [content-type, authorization]
      allowCredentials: true
      maxAge: 3600
```

非法 origin 或 method 是**启动错误**，而不是某个请求悄悄少一个头。

### `contextPath` 与静态文件的位置

`contextPath` 给所有路由加前缀。写成 `api`、`/api/` 还是 `/api` 是同一件事——会归一化成 `/api`；
fallback 查 `locations` 时会把前缀剥掉，所以界面可以放在 `/`、API 放在 `/api`。

`locations` 映射到 URL **根**：每个目录**就是**根，所以 `./public/report.csv` 在 `/report.csv`
提供，而不是 `/public/report.csv`。

### 静态文件

`routes.static_files("/assets", "./public")` 和 `web.server.locations` 都是流式读盘，并完整支持
HTTP 文件协议——`ETag`、`Last-Modified`、`If-None-Match`、`If-Modified-Since`、带 `206`/`416`
的 `Range`、`Accept-Ranges`、目录索引，以及不带 body 的 `HEAD`。请求先 canonicalize 再做包含性
检查，所以 `..` 和符号链接都逃不出根目录。

### 可观测性

每个请求在 `debug` 级记一行，带的是**路由模板**——你看到的是 `/users/:id` 而不是 `/users/42`：

```
DEBUG rust_web_frame::web::server: request method=GET route="/users/:id" path="/users/42" status=200 elapsed_ms=1 client=127.0.0.1
```

用 `log.level: debug` 或更窄的 `rust_web_frame=debug` 打开。失败无论如何都会以 `error` 记录。

panic 会被兜住：客户端拿到标准 `500` 信封（**不会**看到 panic 消息），服务器继续服务，消息以
`error` 记进日志。

两条值得记住的规则：请求体**只能读一次**——先 `multipart()` 再 `read_body()` 会返回一个
明确的 `400`，而不是静默返回空；以及 multipart 请求上的 `req.page()` 从 query string 读
`pageNo`/`pageSize`，因为此时的 body 装的是文件不是 JSON。

---

## 框架里有什么

| 领域 | 类型 |
|---|---|
| **路由** | `Context::get/post/put/delete/patch/any`、`Routes`、`RouteHandle`、Gin 风格的 `:id` 与 `*path`、context path、`static_files`、404/405 信封 |
| **请求** | `Request` —— 路径参数、query、form、JSON 绑定、multipart 文件上传、分页、头、Cookie、客户端 IP、扩展属性、`has_meta` |
| **响应** | `Response`、`Message`（`{code, data, msg, type}`）、`FileResponse`、重定向、原始 body，以及为 handler 常用返回类型实现的 `IntoResponse` |
| **错误** | `Error` 同时承载 HTTP 码与业务码；从 `io::Error`、`sea_orm::DbErr`、`serde_json::Error`、`String`、`&str` 的 `?` 友好转换 |
| **流式** | `SseStream`、`WebSocketStream` |
| **运维** | 访问日志（每请求一行，`debug` 级）、panic 隔离并返回标准 500 信封、请求超时、CORS、带 runner 与连接宽限期的优雅关停 |
| **上传** | `MultipartForm`、`UploadedFile`、`sanitise_file_name`，流式解析并落临时文件 |
| **扩展点** | `Filter`、`Converter`、`Runner`、`Rest`、`Service` |
| **数据** | `db::Database`（连接池**或**事务）、`db::transaction`、`Model<E>`、`define_model!`、`ModelGroup` —— 查询本身交给 [sea-orm](https://docs.rs/sea-orm) |
| **分页** | `Page`、`Paged<T>` |
| **组件** | `Service`、`Rest`、`Filter`、`Runner`、`Converter`、`Context`、`RestGroup`、`Runtime`、`Builder` |

### handler 的返回类型

handler 自己声明返回类型；任何实现了 `IntoResponse` 的类型都行：

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

handler 共享状态有两种方式：捕获已注册组件的 `Arc`，或者按请求查：

```rust
fn service(req: &Request) -> Arc<UserService> {
    req.context().service::<UserService>().expect("registered at startup")
}
```

### 错误映射

| 情况 | HTTP | 信封 `code` |
|---|---|---|
| `Ok(...)` | 200 | 200 |
| `Err(Error::not_found())` | 404 | 404 |
| `Err(Error::forbidden())` | 403 | 403 |
| `Err(Error::token_expired())` | **200** | 1003 |
| `Err("boom".into())` | 500 | 500 |
| 没有路由匹配 | 404 | 404 |
| 路径匹配、方法不匹配 | 405 | 405 |

业务码（1000+）走 HTTP 200，避免浏览器和代理把领域层的失败当成传输层故障——真正的语义在
信封的 `code` 里。

### 多端口

```rust
let admin = RestGroup::builder("admin")
    .server_config(ServerConfig::on_port(8081).with_context_path("/admin"))
    .routes(|ctx| { ctx.get("/stats", handler(stats)); })
    .build();

builder.rest_group(admin);
```

---

## 开发

```bash
cargo test                      # 单元测试、集成测试、文档测试
cargo clippy --all-targets      # 无警告
cargo fmt
cargo run --example rest        # 跑示例
cargo test --features mysql     # 换一个数据库驱动编译
```

feature 开关：`sqlite`（默认）、`mysql`、`postgres`。

---

## 与 Go 原版的不同

设计一致；语言带来几处诚实的差异。

| go-web-frame | rust-web-frame | 原因 |
|---|---|---|
| `return "hello", nil` | 声明返回类型后 `Ok("hello")` | Rust 没有 `any`；`IntoResponse` 覆盖同样的场景而不丢类型。 |
| `errors.New("boom")` | `Err("boom".into())` | 统一一个错误类型 `Error`，为常见来源实现 `From`。 |
| `c.Query("page")` | `req.query("page") -> Option<&str>` | "不存在"体现在类型里；`query_or`、`query_i64` 覆盖顺手的情况。 |
| `Query.One()` 返回零值 + nil error | `query().one() -> Result<Option<T>>` | "有没有查到"由类型回答；`get_by_pk` 则直接返回 404。 |
| `Model.Save(&user)`（upsert） | `insert(am)`、`update_by_pk(am)` | 两个操作两个方法，不在"主键是否设置"上藏分支。 |
| 在 GORM **外面**包一层 `Model[T]` / `EntryModel[T, PK]` | `Model<E>` **绑到** sea-orm 上 | Go 那层是为了给 GORM 补泛型和主键 API；sea-orm 两者都有，再包一层只会把它藏起来。 |
| `Query.Where("age > ?", 18)` | `filter(Column::Age.eq(18))` | 类型化的列在调用处就被检查；字符串条件只能到运行时才报错。 |
| 内嵌 `*model.EntryModel[T, PK]` | `define_model!` → `Deref<Target = Model<E>>` | 用宏写出 Rust 写不出的"内嵌"。 |
| `Viper.UnmarshalKey` | `config.section::<T>("web.server")` | "反序列化一个配置段"不叫 unmarshal key。 |
| `Message{code, data, msg, type}` | `Message{code, data, message, kind}`（serde rename 保持线上格式） | `type` 是关键字，`msg` 是缩写。 |
| `PageAble.List` | `Paged.items`（序列化为 `list`） | —— |
| `gin.HandlerFunc` | `web::Handler` | 按"它是什么"命名，而不是按引入它的那个包。 |
| 按结构体名反射 | 以 `TypeId` 为键的注册表，`ctx.service::<T>()` | Rust 没有运行时反射。 |
| `ctx.Get(...).WithMeta(...)` 返回 `*Route` | `ctx.get(...)` 返回 `RouteHandle`，`with_meta` 消费它 | 惯用的 Rust builder，链式调用不变。 |
| `c.MultipartForm()`（解析进内存，超过 32 MiB 落盘） | `req.multipart().await?`（流式，超过 1 MiB 落盘） | 流式处理让大文件根本不进堆，而不只是"控制在一个阈值以内"。 |

这一版没有实现：验证码、二维码、邮件、反向代理、TLS 监听，以及 `component/` 下的那些组件
（`cache`、`ratelimit` 等）。它们的扩展点——`Filter`、`Service`、`Converter`——都已就位。

---

## 许可证

Apache-2.0。
