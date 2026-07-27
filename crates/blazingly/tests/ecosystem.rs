#![cfg(all(feature = "database", feature = "queue", feature = "templates"))]

use blazingly::prelude::*;
use futures_lite::future;
use serde::Serialize;
use std::convert::Infallible;

#[derive(Clone)]
struct TestPool;

impl ConnectionPool for TestPool {
    type Connection = u64;
    type Error = Infallible;

    fn acquire(&self) -> Result<Self::Connection, Self::Error> {
        Ok(40)
    }
}

#[derive(Serialize)]
struct PageContext {
    answer: u64,
    payload: String,
}

#[get("/ecosystem", id = "ecosystem.integration")]
async fn ecosystem(
    database: Depends<Database<TestPool>>,
    queue: Depends<QueueClient<MemoryQueue>>,
    templates: Depends<Templates>,
) -> Html {
    let answer = database
        .run(|connection| {
            *connection += 2;
            Ok::<_, Infallible>(*connection)
        })
        .await
        .expect("database fixture");
    queue
        .publish("pages", Message::new("<Blazingly>"))
        .await
        .expect("publish fixture");
    let payload = queue
        .receive("pages")
        .await
        .expect("receive fixture")
        .expect("queued fixture");
    queue.ack(&payload.receipt).await.expect("ack fixture");

    templates
        .render(
            "page.html",
            PageContext {
                answer,
                payload: String::from_utf8(payload.message.body).expect("utf-8 fixture"),
            },
        )
        .expect("render fixture")
}

#[test]
fn database_queue_and_templates_are_compiled_di_dependencies() {
    let templates = Templates::compile([(
        "page.html".to_owned(),
        "<main>{{ answer }} {{ payload }}</main>".to_owned(),
    )])
    .expect("compile fixture");
    let executable = ExecutableApp::from_plugin(
        Plugin::new("ecosystem")
            .provide(Provider::value(Database::new(TestPool)))
            .provide(Provider::value(QueueClient::new(MemoryQueue::default())))
            .provide(Provider::value(templates))
            .routes(routes![ecosystem]),
    )
    .expect("ecosystem graph");

    let response = future::block_on(TestApp::new(&executable).call(Request::get("/ecosystem")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.get_header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        response.text().expect("utf-8 response"),
        "<main>42 &lt;Blazingly&gt;</main>"
    );
}
