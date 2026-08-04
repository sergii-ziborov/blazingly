# blazingly-deploy

Container and Kubernetes deployment scaffold generation for a Blazingly
application.

`scaffold_files` renders a `Dockerfile`, `.dockerignore`, and a kustomize
tree — Deployment, ClusterIP Service, HorizontalPodAutoscaler,
PodDisruptionBudget, and two overlays: maintained-NGINX ingress or a direct
`LoadBalancer` Service — from a `KubernetesConfig` (image, ingress host,
replica range, target CPU utilization). It has no dependencies, performs no
I/O, and works standalone as plain string generation; nothing in its API
names a framework type. The output is tailored to the Blazingly native
server: the Deployment sets `BLAZINGLY_LISTEN_ADDRESS`, `BLAZINGLY_WORKERS`,
and `BLAZINGLY_MAX_REQUESTS_PER_CONNECTION`, and the probes expect `/health`
on port 3000. `blazingly-docs` composes it into the generated project
scaffold, and the [Blazingly](https://github.com/sergii-ziborov/blazingly)
framework facade re-exports it as `blazingly::deploy`. The two exposure modes
are compared in
[deployment modes](https://github.com/sergii-ziborov/blazingly/blob/main/docs/deployment.md).

## Direct use

```toml
[dependencies]
blazingly-deploy = "0.2"
```

```rust
use blazingly_deploy::{KubernetesConfig, scaffold_files};

fn main() {
    let config = KubernetesConfig::new("users-api")
        .with_ingress_host("api.example.com")
        .with_replicas(2, 16);
    for (path, contents) in scaffold_files("users-api", &config) {
        println!("{path}: {} bytes", contents.len());
    }
}
```

## Links

- [API documentation](https://docs.rs/blazingly-deploy)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
