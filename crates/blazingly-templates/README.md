# blazingly-templates

MiniJinja HTML templates compiled once and returned as typed, escaped
`text/html` responses in the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework.

`Templates` compiles inline sources or a directory tree into one shared
environment — a natural singleton dependency-injection value — and `render`
returns `Html`, a typed response carrying a status. Every template is
HTML-escaped regardless of its name; `EscapeMode::None` is the only opt-out
and exists for non-HTML output such as plain text or CSV. The crate is fully
usable standalone: compiling and rendering, as in the example, needs no HTTP,
no server, and no facade. The only framework coupling is that `Html`
implements `OperationOutput`, so returning it from a handler produces a
`text/html` response. The facade re-exports the crate as
`blazingly::templates` behind its optional, non-default `templates` feature.

```rust
use blazingly_templates::Templates;
use std::collections::BTreeMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let templates = Templates::compile([(
        "hello.html".to_owned(),
        "Hello, {{ name }}!".to_owned(),
    )])?;
    let page = templates.render("hello.html", BTreeMap::from([("name", "<world>")]))?;
    assert_eq!(page.status(), 200);
    assert_eq!(page.body(), "Hello, &lt;world&gt;!");
    Ok(())
}
```

## Links

- [API documentation](https://docs.rs/blazingly-templates)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
