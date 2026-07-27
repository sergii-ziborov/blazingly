#![forbid(unsafe_code)]

//! Container and Kubernetes deployment scaffold generation.

use std::collections::BTreeMap;

/// Configuration for the generated container and Kubernetes deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KubernetesConfig {
    pub application_name: String,
    pub container_image: String,
    pub ingress_host: String,
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub target_cpu_utilization: u32,
}

impl KubernetesConfig {
    #[must_use]
    pub fn new(package_name: impl AsRef<str>) -> Self {
        let application_name = kubernetes_name(package_name.as_ref());
        Self {
            container_image: format!("{application_name}:latest"),
            ingress_host: format!("{application_name}.example.com"),
            application_name,
            min_replicas: 2,
            max_replicas: 32,
            target_cpu_utilization: 60,
        }
    }

    #[must_use]
    pub fn with_container_image(mut self, image: impl Into<String>) -> Self {
        self.container_image = image.into();
        self
    }

    #[must_use]
    pub fn with_ingress_host(mut self, host: impl Into<String>) -> Self {
        self.ingress_host = host.into();
        self
    }

    #[must_use]
    pub const fn with_replicas(mut self, minimum: u32, maximum: u32) -> Self {
        assert!(minimum > 0, "minimum replicas must be greater than zero");
        assert!(
            maximum >= minimum,
            "maximum replicas must not be lower than the minimum"
        );
        self.min_replicas = minimum;
        self.max_replicas = maximum;
        self
    }

    #[must_use]
    pub const fn with_target_cpu_utilization(mut self, percent: u32) -> Self {
        assert!(
            percent > 0 && percent <= 100,
            "target CPU utilization must be between 1 and 100"
        );
        self.target_cpu_utilization = percent;
        self
    }
}

/// Generates Docker and Kubernetes deployment files for a native application.
#[must_use]
pub fn scaffold_files(package_name: &str, config: &KubernetesConfig) -> BTreeMap<String, String> {
    let name = &config.application_name;
    let mut files = BTreeMap::new();
    files.insert(".dockerignore".to_owned(), dockerignore());
    files.insert(
        "Dockerfile".to_owned(),
        dockerfile(package_name, &config.container_image),
    );
    files.insert("deploy/README.md".to_owned(), deployment_readme(config));
    files.insert(
        "deploy/kubernetes/base/deployment.yaml".to_owned(),
        deployment(config),
    );
    files.insert(
        "deploy/kubernetes/base/service.yaml".to_owned(),
        service(name),
    );
    files.insert("deploy/kubernetes/base/hpa.yaml".to_owned(), hpa(config));
    files.insert(
        "deploy/kubernetes/base/pdb.yaml".to_owned(),
        pod_disruption_budget(name),
    );
    files.insert(
        "deploy/kubernetes/base/kustomization.yaml".to_owned(),
        base_kustomization(),
    );
    files.insert(
        "deploy/kubernetes/overlays/direct/kustomization.yaml".to_owned(),
        direct_kustomization(),
    );
    files.insert(
        "deploy/kubernetes/overlays/direct/service-load-balancer.yaml".to_owned(),
        direct_service_patch(name),
    );
    files.insert(
        "deploy/kubernetes/overlays/nginx/kustomization.yaml".to_owned(),
        nginx_kustomization(),
    );
    files.insert(
        "deploy/kubernetes/overlays/nginx/ingress.yaml".to_owned(),
        nginx_ingress(config),
    );
    files
}

fn dockerignore() -> String {
    "target\n.git\n.gitignore\ndeploy\nREADME.md\n".to_owned()
}

fn dockerfile(package_name: &str, image: &str) -> String {
    format!(
        "# Build and tag this image as {image}\n\
         FROM rust:1.88-bookworm AS builder\n\
         WORKDIR /source\n\
         COPY . .\n\
         RUN cargo build --release\n\n\
         FROM debian:bookworm-slim\n\
         COPY --from=builder /source/target/release/{package_name} /usr/local/bin/blazingly-app\n\
         USER 65532:65532\n\
         EXPOSE 3000\n\
         ENTRYPOINT [\"/usr/local/bin/blazingly-app\"]\n"
    )
}

fn deployment(config: &KubernetesConfig) -> String {
    let name = &config.application_name;
    let image = &config.container_image;
    format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n\
         \x20 name: {name}\n\
         \x20 labels:\n\
         \x20   app.kubernetes.io/name: {name}\n\
         \x20   app.kubernetes.io/managed-by: blazingly\n\
         spec:\n\
         \x20 replicas: {}\n\
         \x20 strategy:\n\
         \x20   type: RollingUpdate\n\
         \x20   rollingUpdate:\n\
         \x20     maxUnavailable: 0\n\
         \x20     maxSurge: 1\n\
         \x20 selector:\n\
         \x20   matchLabels:\n\
         \x20     app.kubernetes.io/name: {name}\n\
         \x20 template:\n\
         \x20   metadata:\n\
         \x20     labels:\n\
         \x20       app.kubernetes.io/name: {name}\n\
         \x20   spec:\n\
         \x20     automountServiceAccountToken: false\n\
         \x20     terminationGracePeriodSeconds: 30\n\
         \x20     securityContext:\n\
         \x20       runAsNonRoot: true\n\
         \x20       seccompProfile:\n\
         \x20         type: RuntimeDefault\n\
         \x20     topologySpreadConstraints:\n\
         \x20       - maxSkew: 1\n\
         \x20         topologyKey: kubernetes.io/hostname\n\
         \x20         whenUnsatisfiable: ScheduleAnyway\n\
         \x20         labelSelector:\n\
         \x20           matchLabels:\n\
         \x20             app.kubernetes.io/name: {name}\n\
         \x20     containers:\n\
         \x20       - name: app\n\
         \x20         image: {image}\n\
         \x20         imagePullPolicy: IfNotPresent\n\
         \x20         env:\n\
         \x20           - name: BLAZINGLY_LISTEN_ADDRESS\n\
         \x20             value: 0.0.0.0:3000\n\
         \x20           - name: BLAZINGLY_WORKERS\n\
         \x20             value: \"1\"\n\
         \x20           - name: BLAZINGLY_MAX_REQUESTS_PER_CONNECTION\n\
         \x20             value: \"10000\"\n\
         \x20         ports:\n\
         \x20           - name: http\n\
         \x20             containerPort: 3000\n\
         \x20         resources:\n\
         \x20           requests:\n\
         \x20             cpu: 500m\n\
         \x20             memory: 64Mi\n\
         \x20           limits:\n\
         \x20             memory: 256Mi\n\
         \x20         securityContext:\n\
         \x20           allowPrivilegeEscalation: false\n\
         \x20           readOnlyRootFilesystem: true\n\
         \x20           capabilities:\n\
         \x20             drop: [\"ALL\"]\n\
         \x20         startupProbe:\n\
         \x20           httpGet:\n\
         \x20             path: /health\n\
         \x20             port: http\n\
         \x20           periodSeconds: 1\n\
         \x20           failureThreshold: 30\n\
         \x20         readinessProbe:\n\
         \x20           httpGet:\n\
         \x20             path: /health\n\
         \x20             port: http\n\
         \x20           periodSeconds: 2\n\
         \x20           failureThreshold: 3\n\
         \x20         livenessProbe:\n\
         \x20           httpGet:\n\
         \x20             path: /health\n\
         \x20             port: http\n\
         \x20           periodSeconds: 10\n\
         \x20           failureThreshold: 3\n",
        config.min_replicas
    )
}

fn service(name: &str) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Service\n\
         metadata:\n\
         \x20 name: {name}\n\
         \x20 labels:\n\
         \x20   app.kubernetes.io/name: {name}\n\
         spec:\n\
         \x20 type: ClusterIP\n\
         \x20 selector:\n\
         \x20   app.kubernetes.io/name: {name}\n\
         \x20 ports:\n\
         \x20   - name: http\n\
         \x20     port: 80\n\
         \x20     targetPort: http\n"
    )
}

fn hpa(config: &KubernetesConfig) -> String {
    let name = &config.application_name;
    format!(
        "apiVersion: autoscaling/v2\n\
         kind: HorizontalPodAutoscaler\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 scaleTargetRef:\n\
         \x20   apiVersion: apps/v1\n\
         \x20   kind: Deployment\n\
         \x20   name: {name}\n\
         \x20 minReplicas: {}\n\
         \x20 maxReplicas: {}\n\
         \x20 behavior:\n\
         \x20   scaleUp:\n\
         \x20     stabilizationWindowSeconds: 0\n\
         \x20     selectPolicy: Max\n\
         \x20     policies:\n\
         \x20       - type: Percent\n\
         \x20         value: 100\n\
         \x20         periodSeconds: 15\n\
         \x20       - type: Pods\n\
         \x20         value: 4\n\
         \x20         periodSeconds: 15\n\
         \x20   scaleDown:\n\
         \x20     stabilizationWindowSeconds: 300\n\
         \x20     policies:\n\
         \x20       - type: Percent\n\
         \x20         value: 25\n\
         \x20         periodSeconds: 60\n\
         \x20 metrics:\n\
         \x20   - type: Resource\n\
         \x20     resource:\n\
         \x20       name: cpu\n\
         \x20       target:\n\
         \x20         type: Utilization\n\
         \x20         averageUtilization: {}\n",
        config.min_replicas, config.max_replicas, config.target_cpu_utilization
    )
}

fn pod_disruption_budget(name: &str) -> String {
    format!(
        "apiVersion: policy/v1\n\
         kind: PodDisruptionBudget\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 minAvailable: 1\n\
         \x20 selector:\n\
         \x20   matchLabels:\n\
         \x20     app.kubernetes.io/name: {name}\n"
    )
}

fn base_kustomization() -> String {
    "apiVersion: kustomize.config.k8s.io/v1beta1\n\
     kind: Kustomization\n\
     resources:\n\
     \x20 - deployment.yaml\n\
     \x20 - service.yaml\n\
     \x20 - hpa.yaml\n\
     \x20 - pdb.yaml\n"
        .to_owned()
}

fn direct_kustomization() -> String {
    "apiVersion: kustomize.config.k8s.io/v1beta1\n\
     kind: Kustomization\n\
     resources:\n\
     \x20 - ../../base\n\
     patches:\n\
     \x20 - path: service-load-balancer.yaml\n"
        .to_owned()
}

fn direct_service_patch(name: &str) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Service\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 type: LoadBalancer\n"
    )
}

fn nginx_kustomization() -> String {
    "apiVersion: kustomize.config.k8s.io/v1beta1\n\
     kind: Kustomization\n\
     resources:\n\
     \x20 - ../../base\n\
     \x20 - ingress.yaml\n"
        .to_owned()
}

fn nginx_ingress(config: &KubernetesConfig) -> String {
    let name = &config.application_name;
    let host = &config.ingress_host;
    format!(
        "apiVersion: networking.k8s.io/v1\n\
         kind: Ingress\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 ingressClassName: nginx\n\
         \x20 rules:\n\
         \x20   - host: {host}\n\
         \x20     http:\n\
         \x20       paths:\n\
         \x20         - path: /\n\
         \x20           pathType: Prefix\n\
         \x20           backend:\n\
         \x20             service:\n\
         \x20               name: {name}\n\
         \x20               port:\n\
         \x20                 number: 80\n"
    )
}

fn deployment_readme(config: &KubernetesConfig) -> String {
    let image = &config.container_image;
    let host = &config.ingress_host;
    format!(
        "# Kubernetes deployment\n\n\
         Build and publish the application image:\n\n\
         ```sh\n\
         docker build -t {image} .\n\
         docker push {image}\n\
         ```\n\n\
         ## Direct mode\n\n\
         This bypasses an HTTP ingress controller and exposes the Blazingly\n\
         Service through the cluster's `LoadBalancer` implementation:\n\n\
         ```sh\n\
         kubectl apply -k deploy/kubernetes/overlays/direct\n\
         ```\n\n\
         ## NGINX mode\n\n\
         This keeps the Service internal and routes `{host}` through an\n\
         already installed, maintained controller that owns ingress class\n\
         `nginx`:\n\n\
         ```sh\n\
         kubectl apply -k deploy/kubernetes/overlays/nginx\n\
         ```\n\n\
         Do not install the retired community `ingress-nginx` controller for a\n\
         new cluster. Use a maintained NGINX implementation or change\n\
         `ingressClassName` to the controller selected by the platform team.\n\n\
         Both modes use the same `autoscaling/v2` HPA: {} to {} pods, target\n\
         CPU utilization {}%, immediate scale-up with up to 2x/4 extra pods\n\
         per 15 seconds, and a five-minute scale-down stabilization window.\n\
         CPU autoscaling requires Kubernetes resource metrics (commonly\n\
         metrics-server). Scaling cluster nodes additionally requires the\n\
         cloud provider's node autoscaler, Cluster Autoscaler, or Karpenter.\n\n\
         Kubernetes selects a pod per TCP connection, not per HTTP/1 request.\n\
         The Deployment therefore sets\n\
         `BLAZINGLY_MAX_REQUESTS_PER_CONNECTION=10000` so very old keep-alive\n\
         connections eventually reconnect and can reach newly added pods.\n",
        config.min_replicas, config.max_replicas, config.target_cpu_utilization
    )
}

fn kubernetes_name(package_name: &str) -> String {
    let mut name = String::with_capacity(package_name.len().min(63));
    let mut separator = false;
    for character in package_name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !name.is_empty() && name.len() < 63 {
                name.push('-');
            }
            separator = false;
            if name.len() < 63 {
                name.push(character.to_ascii_lowercase());
            }
        } else {
            separator = true;
        }
    }
    while name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() {
        "blazingly-app".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::{KubernetesConfig, kubernetes_name, scaffold_files};

    #[test]
    fn names_are_valid_stable_dns_labels() {
        assert_eq!(
            kubernetes_name("Hello_Blazingly.API"),
            "hello-blazingly-api"
        );
        assert_eq!(kubernetes_name("___"), "blazingly-app");
        assert!(kubernetes_name(&"A".repeat(80)).len() <= 63);
    }

    #[test]
    fn scaffold_contains_both_exposure_modes_and_shared_autoscaling() {
        let config = KubernetesConfig::new("hello_blazingly")
            .with_container_image("registry.example/hello:v1")
            .with_ingress_host("api.example.com")
            .with_replicas(3, 40)
            .with_target_cpu_utilization(55);
        let files = scaffold_files("hello_blazingly", &config);

        assert!(files["deploy/kubernetes/base/hpa.yaml"].contains("apiVersion: autoscaling/v2"));
        assert!(files["deploy/kubernetes/base/hpa.yaml"].contains("maxReplicas: 40"));
        assert!(
            files["deploy/kubernetes/overlays/direct/service-load-balancer.yaml"]
                .contains("type: LoadBalancer")
        );
        assert!(
            files["deploy/kubernetes/overlays/nginx/ingress.yaml"]
                .contains("ingressClassName: nginx")
        );
        assert!(files["deploy/kubernetes/overlays/nginx/ingress.yaml"].contains("api.example.com"));
        assert!(
            files["deploy/kubernetes/base/deployment.yaml"].contains("registry.example/hello:v1")
        );
    }
}
