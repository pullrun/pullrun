# Prometheus Operator manifests for Pullrun runtime observability.
#
# These manifests assume the Prometheus Operator is installed in the
# `monitoring` namespace (the kube-prometheus-stack default). Adjust
# `namespace` and the `release: kube-prometheus-stack` selector if your
# cluster uses different conventions.
#
# Apply order:
#   kubectl apply -f deploy/serviceaccount.yaml
#   kubectl apply -f deploy/servicemonitor.yaml
#   kubectl apply -f deploy/prometheusrule.yaml
#   kubectl apply -f deploy/runtime-daemon.yaml
#
# Verify:
#   kubectl get servicemonitor -n pullrun
#   kubectl get prometheusrule -n pullrun
#   kubectl port-forward -n pullrun svc/pullrun-runtime 9090:9090
#   curl http://localhost:9090/metrics | head
