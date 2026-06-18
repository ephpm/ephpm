{{/*
Input validation. Invoked from configmap.yaml (always rendered) so the guards
actually fire — a partial that only self-includes never runs.
*/}}
{{- define "ephpm.validate" -}}

{{- if and .Values.cluster.enabled (lt (int .Values.cluster.replicas) 2) }}
{{- fail "cluster.enabled=true requires cluster.replicas >= 2" }}
{{- end }}

{{- if and .Values.autoscaling.enabled (include "ephpm.isStatefulSet" .) }}
{{- fail "autoscaling.enabled is only supported for Deployment workloads (cluster.enabled and db.sqlite.enabled must be false)" }}
{{- end }}

{{- if and .Values.persistence.enabled (include "ephpm.isStatefulSet" .) }}
{{- fail "persistence.enabled (document-root PVC) is only for Deployment mode; with clustering/sqlite, bake the app into the image" }}
{{- end }}

{{- if and .Values.db.mysql.enabled (not .Values.db.mysql.url) (not .Values.db.mysql.existingSecret) }}
{{- fail "db.mysql.enabled=true requires db.mysql.url or db.mysql.existingSecret" }}
{{- end }}

{{- if and .Values.db.postgres.enabled (not .Values.db.postgres.url) (not .Values.db.postgres.existingSecret) }}
{{- fail "db.postgres.enabled=true requires db.postgres.url or db.postgres.existingSecret" }}
{{- end }}

{{- if and .Values.db.readWriteSplit.enabled .Values.db.mysql.enabled (not .Values.db.mysql.replicas.urls) }}
{{- fail "db.readWriteSplit.enabled with db.mysql requires db.mysql.replicas.urls" }}
{{- end }}

{{- if and .Values.db.readWriteSplit.enabled .Values.db.postgres.enabled (not .Values.db.postgres.replicas.urls) }}
{{- fail "db.readWriteSplit.enabled with db.postgres requires db.postgres.replicas.urls" }}
{{- end }}

{{- if and .Values.tls.enabled (eq .Values.tls.mode "manual") (not .Values.tls.secretName) }}
{{- fail "tls.mode=manual requires tls.secretName (a kubernetes.io/tls Secret with tls.crt and tls.key)" }}
{{- end }}

{{- if and .Values.tls.enabled (eq .Values.tls.mode "acme") (not .Values.tls.acme.domains) }}
{{- fail "tls.mode=acme requires tls.acme.domains" }}
{{- end }}

{{- end }}
