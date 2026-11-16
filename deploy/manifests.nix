{
  pkgs,
  lib,
}: let
  yaml = pkgs.formats.yaml_1_2 {};

  namespace = "abembed";

  image = name: "ghcr.io/iaverages/abembed/${name}";
  labels = name: {"app.kubernetes.io/name" = name;};

  metadata = name: {
    inherit name namespace;
    labels = labels name;
  };

  deployment = name: podSpec: {
    apiVersion = "apps/v1";
    kind = "Deployment";
    metadata = metadata name;
    spec = {
      replicas = 1;
      selector.matchLabels = labels name;
      template = {
        metadata.labels = labels name;
        spec = podSpec;
      };
    };
  };
  service = name: port: {
    apiVersion = "v1";
    kind = "Service";
    metadata = metadata name;
    spec = {
      type = "ClusterIP";
      selector = labels name;
      ports = [
        {
          name = "http";
          inherit port;
          targetPort = "http";
          protocol = "TCP";
        }
      ];
    };
  };
  ingress = name: host: port: {
    apiVersion = "networking.k8s.io/v1";
    kind = "Ingress";
    metadata = metadata name;
    spec = {
      ingressClassName = "traefik";
      rules = [
        {
          inherit host;
          http.paths = [
            {
              path = "/";
              pathType = "Prefix";
              backend.service = {
                inherit name;
                port.number = port;
              };
            }
          ];
        }
      ];
    };
  };

  httpPort = containerPort: {
    name = "http";
    inherit containerPort;
    protocol = "TCP";
  };
  healthProbe = {
    httpGet = {
      path = "/health";
      port = "http";
    };
    timeoutSeconds = 2;
  };
  readinessProbe =
    healthProbe
    // {
      initialDelaySeconds = 5;
      periodSeconds = 10;
      failureThreshold = 3;
    };
  livenessProbe =
    readinessProbe
    // {
      initialDelaySeconds = 15;
      periodSeconds = 20;
    };

  manifests = {
    "api/deployment.yaml" = deployment "api" {
      containers = [
        {
          name = "api";
          image = image "api";
          imagePullPolicy = "IfNotPresent";
          ports = [(httpPort 3000)];
          envFrom = [{secretRef.name = "abembed-api-secrets";}];
          env = [
            {
              name = "APP_URL";
              value = "https://open.spqtify.com";
            }
            {
              name = "MACHINA_VIDEO_GENERATION_DIR";
              value = "/tmp/machina";
            }
            {
              name = "SPQTIFY_DEFAULT_BASE_COLOR";
              value = "#000";
            }
            {
              name = "EMBED_IMAGE_SERVICE_URL";
              value = "http://embed-image-service:3001";
            }
            {
              name = "POSTHOG_HOST";
              value = "https://eu.i.posthog.com";
            }
          ];
          inherit livenessProbe readinessProbe;
          resources = {
            requests = {
              cpu = "1";
              memory = "512Mi";
            };
            limits = {
              cpu = "4";
              memory = "2Gi";
            };
          };
        }
      ];
    };
    "api/service.yaml" = service "api" 3000;
    "api/ingress.yaml" = ingress "api" "open.spqtify.com" 3000;

    "embed-image-service/deployment.yaml" = deployment "embed-image-service" {
      containers = [
        {
          name = "embed-image-service";
          image = image "embed-image-service";
          imagePullPolicy = "IfNotPresent";
          ports = [(httpPort 3001)];
          inherit livenessProbe readinessProbe;
          resources = {
            requests = {
              cpu = "200m";
              memory = "256Mi";
            };
            limits = {
              cpu = "1000m";
              memory = "1Gi";
            };
          };
        }
      ];
    };
    "embed-image-service/service.yaml" = service "embed-image-service" 3001;
    "embed-image-service/network-policy.yaml" = {
      apiVersion = "networking.k8s.io/v1";
      kind = "NetworkPolicy";
      metadata = metadata "embed-image-service";
      spec = {
        podSelector.matchLabels = labels "embed-image-service";
        policyTypes = ["Ingress"];
        ingress = [
          {
            from = [{podSelector.matchLabels = labels "api";}];
            ports = [
              {
                protocol = "TCP";
                port = 3001;
              }
            ];
          }
        ];
      };
    };

    "gif-service/deployment.yaml" = deployment "gif-service" {
      securityContext.seccompProfile.type = "RuntimeDefault";
      containers = [
        {
          name = "gif-service";
          image = image "gif-service";
          imagePullPolicy = "IfNotPresent";
          ports = [(httpPort 3002)];
          readinessProbe = healthProbe // {periodSeconds = 10;};
          livenessProbe =
            healthProbe
            // {
              initialDelaySeconds = 10;
              periodSeconds = 20;
            };
          resources = {
            requests = {
              cpu = "200m";
              memory = "256Mi";
            };
            limits = {
              cpu = "4";
              memory = "4Gi";
            };
          };
          securityContext = {
            allowPrivilegeEscalation = false;
            readOnlyRootFilesystem = true;
            runAsNonRoot = true;
            runAsUser = 65532;
            runAsGroup = 65532;
            capabilities.drop = ["ALL"];
          };
        }
      ];
    };
    "gif-service/service.yaml" = service "gif-service" 3002;
    "gif-service/ingress.yaml" = ingress "gif-service" "gif.abembed.com" 3002;
  };

  files =
    lib.mapAttrs (path: yaml.generate (baseNameOf path)) manifests
    // {
      "kustomization.yaml" = yaml.generate "kustomization.yaml" {
        apiVersion = "kustomize.config.k8s.io/v1beta1";
        kind = "Kustomization";
        resources = builtins.attrNames manifests;
      };
    };
in
  pkgs.runCommand "manifests" {} (
    lib.concatStrings (lib.mapAttrsToList (path: file: "install -Dm0644 ${file} $out/${path}\n") files)
  )
