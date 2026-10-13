# rust-web-frame

**把策略声明在路由上，而不是散落在 handler 里。用 `.with_meta(...)` 给路由打标签——鉴权、权限、限流——一个 filter 为所有"声明过"的路由统一执法。自带泛型 ORM。**

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

### 2. 消除 CRUD 样板代码的泛型 Model

实体写一次；`define_model!` 生成一个带全部查询方法的强类型 model：

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

这就是 model 的全部。然后到处都能用：

```rust
let users = UserModel::new(db);

users.create_table().await?;                       // 从实体推导建表语句
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

**字符串条件 + 绑定参数。** `Where("age > ?", 18)` 的写法与 Go 原版完全一致。`?` 是所有后端
都认识的占位符——sea-query 会为 PostgreSQL 改写成 `$1`——所以既没有 SQL 注入面，也不用为
每种方言写代码。

**聚合查询。** `SUM`、`COUNT`、`AVG`、`GROUP BY`、`HAVING`：

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

**事务里仍是同一套 model API。** 闭包拿到的是一个 `Database` 句柄，由它构造的 model 都参与
这个事务：

```rust
db.transaction(|tx| async move {
    let accounts = AccountModel::new(tx);
    accounts.update().where_pk(from).set("balance", lower).exec().await?;
    accounts.update().where_pk(to).set("balance", higher).exec().await?;
    Ok(())                       // Ok 提交，Err 回滚
})
.await?;
```

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

let section: AppSection = config.unmarshal_key("app")?;
```

---

## 框架里有什么

| 领域 | 类型 |
|---|---|
| **路由** | `Context::get/post/put/delete/patch/any`、`Routes`、`RouteHandle`、Gin 风格的 `:id` 与 `*path`、context path、`static_files`、404/405 信封 |
| **请求** | `Request` —— 路径参数、query、form、JSON 绑定、分页、头、Cookie、客户端 IP、扩展属性、`has_meta` |
| **响应** | `Response`、`Message`（`{code, data, msg, type}`）、`FileResponse`、重定向、原始 body，以及为 handler 常用返回类型实现的 `IntoResponse` |
| **错误** | `Error` 同时承载 HTTP 码与业务码；从 `io::Error`、`sea_orm::DbErr`、`serde_json::Error`、`String`、`&str` 的 `?` 友好转换 |
| **流式** | `SseStream`、`WebSocketStream` |
| **扩展点** | `Filter`、`Converter`、`Runner`、`Rest`、`Service` |
| **数据** | `db::Database`、`db::transaction`、`Model<E>`、`EntryModel<E>`、`define_model!`、`ModelGroup`、`Transaction` |
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
| 内嵌 `*model.EntryModel[T, PK]` | `define_model!` → `Deref<Target = EntryModel<E>>` | 用宏写出 Rust 写不出的"内嵌"。 |
| 按结构体名反射 | 以 `TypeId` 为键的注册表，`ctx.service::<T>()` | Rust 没有运行时反射。 |
| `ctx.Get(...).WithMeta(...)` 返回 `*Route` | `ctx.get(...)` 返回 `RouteHandle`，`with_meta` 消费它 | 惯用的 Rust builder，链式调用不变。 |
| `pageAble` | `Paged<T>` | —— |

这一版没有实现：验证码、二维码、邮件、反向代理，以及 `component/` 下的那些组件
（`cache`、`ratelimit` 等）。它们的扩展点——`Filter`、`Service`、`Converter`——都已就位。

---

## 许可证

Apache-2.0。
