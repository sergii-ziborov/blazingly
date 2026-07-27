# Deployment modes

Blazingly does not hide a cluster scheduler inside the HTTP framework. Axum,
Actix Web, Fastify, and Blazingly can all run multiple workers inside one
process, but creating pods and placing them on nodes is the orchestrator's job.

The project scaffold generates one hardened application deployment with two
network overlays:

```text
NGINX mode:  client -> maintained NGINX controller -> ClusterIP -> pods
Direct mode: client -> LoadBalancer Service --------------------> pods
                                                          |
                                                          +-> HPA: 2..32 pods
```

Both overlays use the same `autoscaling/v2` HorizontalPodAutoscaler. The
default target is 60% CPU against a declared 500m CPU request. Scale-up has no
stabilization delay and may add the larger of 100% or four pods every 15
seconds. Scale-down waits five minutes to avoid oscillation.

Generate a starter:

```rust
let files = blazingly::docs::scaffold(
    &blazingly::docs::ScaffoldConfig::new("users-api").with_kubernetes(
        blazingly::deploy::KubernetesConfig::new("users-api")
            .with_container_image("registry.example/users-api:v1")
            .with_ingress_host("users.example.com")
            .with_replicas(2, 64)
            .with_target_cpu_utilization(60),
    ),
);
```

`KubernetesConfig` and deployment file generation live in the independent
`blazingly-deploy` workspace crate. `blazingly-docs` re-exports the config for
source compatibility and only composes those files into the wider project
scaffold.

Apply one overlay:

```sh
kubectl apply -k deploy/kubernetes/overlays/nginx
kubectl apply -k deploy/kubernetes/overlays/direct
```

The direct overlay requires a cloud or bare-metal `LoadBalancer`
implementation. The NGINX overlay requires an already installed, maintained
controller that owns ingress class `nginx`. The community Kubernetes
`ingress-nginx` project was retired in March 2026 and must not be selected for
a new deployment.

CPU autoscaling requires the resource metrics API, usually provided by
metrics-server. Adding pods beyond current node capacity also requires a node
autoscaler such as the provider's managed autoscaler, Cluster Autoscaler, or
Karpenter.

The generated app uses one native worker per pod by default, so Kubernetes can
spread load predictably. `BLAZINGLY_WORKERS` can increase per-pod parallelism
when the pod receives multiple CPU cores. `BLAZINGLY_LISTEN_ADDRESS` defaults
locally to `127.0.0.1:3000` and is set to `0.0.0.0:3000` by the Deployment.
The Deployment also recycles a keep-alive connection after 10,000 requests:
Kubernetes balances TCP connections, not individual HTTP/1 requests, so
unbounded old connections could otherwise remain pinned to the original pods
after scale-out. This limit is configurable through
`BLAZINGLY_MAX_REQUESTS_PER_CONNECTION`.

Kubernetes sends `SIGTERM` during pod replacement. The scaffold installs
Blazingly's process termination channel and gives the native server 25 seconds
to stop accepting, close keep-alive connections after their current response,
and drain work within the 30-second pod grace period.

Request-rate autoscaling is a later metrics projection. It requires a
`custom.metrics.k8s.io` or `external.metrics.k8s.io` adapter; the framework
must not pretend that an HTTP counter can create pods without that cluster
integration.

References:

- [Kubernetes Horizontal Pod Autoscaling](https://kubernetes.io/docs/concepts/workloads/autoscaling/horizontal-pod-autoscale/)
- [Kubernetes ingress-nginx retirement](https://kubernetes.io/blog/2025/11/11/ingress-nginx-retirement/)
- [F5 NGINX Ingress class configuration](https://docs.nginx.com/nginx-ingress-controller/install/multiple-controllers/)
