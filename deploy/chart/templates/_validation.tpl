{{- define "ephpm.validate" -}}

{{- if and .Values.cluster.enabled (lt (int .Values.replicaCount) 2) }}
{{- fail "cluster.enabled=true requires replicaCount >= 2" }}
{{- end }}

{{- if and .Values.autoscaling.enabled (include "ephpm.isStatefulSet" .) }}
{{- fail "autoscaling.enabled=true is only supported for Deployment workloads (set cluster.enabled=false)" }}
{{- end }}

{{- if and .Values.server.tls.enabled (eq .Values.server.tls.mode "manual") (not .Values.server.tls.secretName) }}
{{- fail "server.tls.mode=manual requires server.tls.secretName (a Secret with tls.crt and tls.key)" }}
{{- end }}

{{- if and .Values.server.tls.enabled (eq .Values.server.tls.mode "acme") (not .Values.server.tls.acme.domains) }}
{{- fail "server.tls.mode=acme requires server.tls.acme.domains" }}
{{- end }}

{{- if and .Values.observability.otlpExport.enabled (not .Values.observability.otlpExport.endpoint) }}
{{- fail "observability.otlpExport.enabled=true requires observability.otlpExport.endpoint" }}
{{- end }}

{{- if and .Values.db.readWriteSplit.enabled .Values.db.mysql.enabled (not .Values.db.mysql.replicas.urls) }}
{{- fail "db.readWriteSplit.enabled=true with db.mysql.enabled requires db.mysql.replicas.urls" }}
{{- end }}

{{- if and .Values.db.readWriteSplit.enabled .Values.db.postgres.enabled (not .Values.db.postgres.replicas.urls) }}
{{- fail "db.readWriteSplit.enabled=true with db.postgres.enabled requires db.postgres.replicas.urls" }}
{{- end }}

{{- if and .Values.db.mysql.enabled (not .Values.db.mysql.url) (not .Values.db.mysql.existingSecret) }}
{{- fail "db.mysql.enabled=true requires either db.mysql.url or db.mysql.existingSecret" }}
{{- end }}

{{- if and .Values.db.postgres.enabled (not .Values.db.postgres.url) (not .Values.db.postgres.existingSecret) }}
{{- fail "db.postgres.enabled=true requires either db.postgres.url or db.postgres.existingSecret" }}
{{- end }}

{{- if and .Values.db.tds.enabled (not .Values.db.tds.url) (not .Values.db.tds.existingSecret) }}
{{- fail "db.tds.enabled=true requires either db.tds.url or db.tds.existingSecret" }}
{{- end }}

{{- end }}
{{- include "ephpm.validate" . }}
