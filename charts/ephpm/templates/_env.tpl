{{/*
All operational EPHPM_ env vars (metrics, db, kv, cluster, tls).
Field names mirror crates/ephpm-config/src/lib.rs (snake_case, "__" nesting).
Include under a container `env:` with `nindent 8`.
*/}}
{{- define "ephpm.configEnv" -}}
- name: EPHPM_SERVER__METRICS__ENABLED
  value: {{ .Values.metrics.enabled | quote }}
- name: EPHPM_SERVER__METRICS__PATH
  value: {{ .Values.metrics.path | quote }}
{{- if .Values.db.mysql.enabled }}
{{- if .Values.db.mysql.existingSecret }}
- name: EPHPM_DB__MYSQL__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.db.mysql.existingSecret }}
      key: {{ .Values.db.mysql.secretKey }}
{{- else }}
- name: EPHPM_DB__MYSQL__URL
  value: {{ .Values.db.mysql.url | quote }}
{{- end }}
- name: EPHPM_DB__MYSQL__LISTEN
  value: {{ .Values.db.mysql.listen | quote }}
- name: EPHPM_DB__MYSQL__MIN_CONNECTIONS
  value: {{ .Values.db.mysql.minConnections | quote }}
- name: EPHPM_DB__MYSQL__MAX_CONNECTIONS
  value: {{ .Values.db.mysql.maxConnections | quote }}
- name: EPHPM_DB__MYSQL__INJECT_ENV
  value: {{ .Values.db.mysql.injectEnv | quote }}
- name: EPHPM_DB__MYSQL__RESET_STRATEGY
  value: {{ .Values.db.mysql.resetStrategy | quote }}
{{- if .Values.db.mysql.replicas.urls }}
- name: EPHPM_DB__MYSQL__REPLICAS__URLS
  value: {{ .Values.db.mysql.replicas.urls | toJson | quote }}
{{- end }}
{{- end }}
{{- if .Values.db.postgres.enabled }}
{{- if .Values.db.postgres.existingSecret }}
- name: EPHPM_DB__POSTGRES__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.db.postgres.existingSecret }}
      key: {{ .Values.db.postgres.secretKey }}
{{- else }}
- name: EPHPM_DB__POSTGRES__URL
  value: {{ .Values.db.postgres.url | quote }}
{{- end }}
- name: EPHPM_DB__POSTGRES__LISTEN
  value: {{ .Values.db.postgres.listen | quote }}
- name: EPHPM_DB__POSTGRES__MIN_CONNECTIONS
  value: {{ .Values.db.postgres.minConnections | quote }}
- name: EPHPM_DB__POSTGRES__MAX_CONNECTIONS
  value: {{ .Values.db.postgres.maxConnections | quote }}
- name: EPHPM_DB__POSTGRES__INJECT_ENV
  value: {{ .Values.db.postgres.injectEnv | quote }}
- name: EPHPM_DB__POSTGRES__RESET_STRATEGY
  value: {{ .Values.db.postgres.resetStrategy | quote }}
{{- if .Values.db.postgres.replicas.urls }}
- name: EPHPM_DB__POSTGRES__REPLICAS__URLS
  value: {{ .Values.db.postgres.replicas.urls | toJson | quote }}
{{- end }}
{{- end }}
{{- if .Values.db.readWriteSplit.enabled }}
- name: EPHPM_DB__READ_WRITE_SPLIT__ENABLED
  value: "true"
- name: EPHPM_DB__READ_WRITE_SPLIT__STRATEGY
  value: {{ .Values.db.readWriteSplit.strategy | quote }}
- name: EPHPM_DB__READ_WRITE_SPLIT__STICKY_DURATION
  value: {{ .Values.db.readWriteSplit.stickyDuration | quote }}
- name: EPHPM_DB__READ_WRITE_SPLIT__MAX_REPLICA_LAG
  value: {{ .Values.db.readWriteSplit.maxReplicaLag | quote }}
{{- end }}
{{- if .Values.db.sqlite.enabled }}
- name: EPHPM_DB__SQLITE__PATH
  value: "/var/lib/ephpm/sqlite/{{ .Values.db.sqlite.path }}"
- name: EPHPM_DB__SQLITE__PROXY__MYSQL_LISTEN
  value: {{ .Values.db.sqlite.proxy.mysqlListen | quote }}
{{- if .Values.cluster.enabled }}
- name: EPHPM_DB__SQLITE__SQLD__HTTP_LISTEN
  value: {{ .Values.db.sqlite.sqld.httpListen | quote }}
- name: EPHPM_DB__SQLITE__SQLD__GRPC_LISTEN
  value: {{ .Values.db.sqlite.sqld.grpcListen | quote }}
- name: EPHPM_DB__SQLITE__REPLICATION__ROLE
  value: {{ .Values.db.sqlite.replication.role | quote }}
{{- if .Values.db.sqlite.replication.primaryGrpcUrl }}
- name: EPHPM_DB__SQLITE__REPLICATION__PRIMARY_GRPC_URL
  value: {{ .Values.db.sqlite.replication.primaryGrpcUrl | quote }}
{{- end }}
{{- end }}
{{- end }}
- name: EPHPM_KV__MEMORY_LIMIT
  value: {{ .Values.kv.memoryLimit | quote }}
- name: EPHPM_KV__EVICTION_POLICY
  value: {{ .Values.kv.evictionPolicy | quote }}
{{- if or .Values.kv.secret .Values.kv.existingSecret }}
{{- if .Values.kv.existingSecret }}
- name: EPHPM_KV__SECRET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.kv.existingSecret }}
      key: {{ .Values.kv.secretKey }}
{{- else }}
- name: EPHPM_KV__SECRET
  value: {{ .Values.kv.secret | quote }}
{{- end }}
{{- end }}
{{- if .Values.kv.redisCompat.enabled }}
- name: EPHPM_KV__REDIS_COMPAT__ENABLED
  value: "true"
- name: EPHPM_KV__REDIS_COMPAT__LISTEN
  value: {{ .Values.kv.redisCompat.listen | quote }}
{{- if or .Values.kv.redisCompat.password .Values.kv.redisCompat.existingSecret }}
{{- if .Values.kv.redisCompat.existingSecret }}
- name: EPHPM_KV__REDIS_COMPAT__PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ .Values.kv.redisCompat.existingSecret }}
      key: {{ .Values.kv.redisCompat.passwordKey }}
{{- else }}
- name: EPHPM_KV__REDIS_COMPAT__PASSWORD
  value: {{ .Values.kv.redisCompat.password | quote }}
{{- end }}
{{- end }}
{{- end }}
{{- if .Values.cluster.enabled }}
- name: EPHPM_CLUSTER__ENABLED
  value: "true"
- name: EPHPM_CLUSTER__BIND
  value: "0.0.0.0:{{ .Values.cluster.gossipPort }}"
- name: EPHPM_CLUSTER__CLUSTER_ID
  value: {{ .Values.cluster.clusterId | default (include "ephpm.fullname" .) | quote }}
- name: EPHPM_CLUSTER__JOIN
  value: {{ include "ephpm.gossipJoin" . | quote }}
{{- if or .Values.cluster.secret .Values.cluster.existingSecret }}
- name: EPHPM_CLUSTER__SECRET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.cluster.existingSecret | default (printf "%s-cluster" (include "ephpm.fullname" .)) }}
      key: {{ .Values.cluster.secretKey }}
{{- end }}
- name: EPHPM_CLUSTER__KV__REPLICATION_FACTOR
  value: {{ .Values.cluster.kv.replicationFactor | quote }}
- name: EPHPM_CLUSTER__KV__REPLICATION_MODE
  value: {{ .Values.cluster.kv.replicationMode | quote }}
- name: EPHPM_CLUSTER__KV__DATA_PORT
  value: {{ .Values.cluster.kv.dataPort | quote }}
{{- end }}
{{- if .Values.tls.enabled }}
{{- if eq .Values.tls.mode "acme" }}
- name: EPHPM_SERVER__TLS__DOMAINS
  value: {{ .Values.tls.acme.domains | toJson | quote }}
{{- if .Values.tls.acme.email }}
- name: EPHPM_SERVER__TLS__EMAIL
  value: {{ .Values.tls.acme.email | quote }}
{{- end }}
- name: EPHPM_SERVER__TLS__CACHE_DIR
  value: {{ .Values.tls.acme.cacheDir | quote }}
- name: EPHPM_SERVER__TLS__STAGING
  value: {{ .Values.tls.acme.staging | quote }}
{{- else }}
- name: EPHPM_SERVER__TLS__CERT
  value: "{{ .Values.tls.mountPath }}/tls.crt"
- name: EPHPM_SERVER__TLS__KEY
  value: "{{ .Values.tls.mountPath }}/tls.key"
{{- end }}
{{- if .Values.tls.listen }}
- name: EPHPM_SERVER__TLS__LISTEN
  value: {{ .Values.tls.listen | quote }}
{{- end }}
- name: EPHPM_SERVER__TLS__REDIRECT_HTTP
  value: {{ .Values.tls.redirectHttp | quote }}
{{- end }}
{{- end }}
