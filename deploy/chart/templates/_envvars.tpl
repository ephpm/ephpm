{{/*
All EPHPM_ environment variables.
Field names and defaults sourced from crates/ephpm-config/src/lib.rs.
Env var format: EPHPM_<SECTION>__<KEY> with double-underscore separators.
Confirmed from k8s/base/ephpm-single.yaml and ephpm-cluster.yaml tests.
*/}}
{{- define "ephpm.envvars" -}}
- name: EPHPM_SERVER__LISTEN
  value: {{ .Values.server.listen | quote }}
- name: EPHPM_SERVER__DOCUMENT_ROOT
  value: {{ .Values.server.documentRoot | quote }}
{{- if .Values.server.sitesDir }}
- name: EPHPM_SERVER__SITES_DIR
  value: {{ .Values.server.sitesDir | quote }}
{{- end }}
- name: EPHPM_SERVER__REQUEST__MAX_BODY_SIZE
  value: {{ .Values.server.request.maxBodySize | quote }}
- name: EPHPM_SERVER__REQUEST__MAX_HEADER_SIZE
  value: {{ .Values.server.request.maxHeaderSize | quote }}
- name: EPHPM_SERVER__TIMEOUTS__HEADER_READ
  value: {{ .Values.server.timeouts.headerRead | quote }}
- name: EPHPM_SERVER__TIMEOUTS__IDLE
  value: {{ .Values.server.timeouts.idle | quote }}
- name: EPHPM_SERVER__TIMEOUTS__REQUEST
  value: {{ .Values.server.timeouts.request | quote }}
- name: EPHPM_SERVER__TIMEOUTS__SHUTDOWN
  value: {{ .Values.server.timeouts.shutdown | quote }}
- name: EPHPM_SERVER__RESPONSE__COMPRESSION
  value: {{ .Values.server.response.compression | quote }}
- name: EPHPM_SERVER__RESPONSE__COMPRESSION_LEVEL
  value: {{ .Values.server.response.compressionLevel | quote }}
- name: EPHPM_SERVER__RESPONSE__COMPRESSION_MIN_SIZE
  value: {{ .Values.server.response.compressionMinSize | quote }}
- name: EPHPM_SERVER__STATIC__HIDDEN_FILES
  value: {{ .Values.server.static.hiddenFiles | quote }}
- name: EPHPM_SERVER__STATIC__ETAG
  value: {{ .Values.server.static.etag | quote }}
{{- if .Values.server.static.cacheControl }}
- name: EPHPM_SERVER__STATIC__CACHE_CONTROL
  value: {{ .Values.server.static.cacheControl | quote }}
{{- end }}
- name: EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED
  value: {{ .Values.server.phpEtagCache.enabled | quote }}
- name: EPHPM_SERVER__PHP_ETAG_CACHE__TTL_SECS
  value: {{ .Values.server.phpEtagCache.ttlSecs | quote }}
- name: EPHPM_SERVER__PHP_ETAG_CACHE__KEY_PREFIX
  value: {{ .Values.server.phpEtagCache.keyPrefix | quote }}
- name: EPHPM_SERVER__SECURITY__OPEN_BASEDIR
  value: {{ .Values.server.security.openBasedir | quote }}
- name: EPHPM_SERVER__SECURITY__DISABLE_SHELL_EXEC
  value: {{ .Values.server.security.disableShellExec | quote }}
- name: EPHPM_SERVER__LOGGING__LEVEL
  value: {{ .Values.server.logging.level | quote }}
{{- if .Values.server.logging.access }}
- name: EPHPM_SERVER__LOGGING__ACCESS
  value: {{ .Values.server.logging.access | quote }}
{{- end }}
- name: EPHPM_SERVER__METRICS__ENABLED
  value: {{ .Values.server.metrics.enabled | quote }}
- name: EPHPM_SERVER__METRICS__PATH
  value: {{ .Values.server.metrics.path | quote }}
- name: EPHPM_SERVER__LIMITS__MAX_CONNECTIONS
  value: {{ .Values.server.limits.maxConnections | quote }}
- name: EPHPM_SERVER__LIMITS__PER_IP_MAX_CONNECTIONS
  value: {{ .Values.server.limits.perIpMaxConnections | quote }}
- name: EPHPM_SERVER__LIMITS__PER_IP_RATE
  value: {{ .Values.server.limits.perIpRate | quote }}
- name: EPHPM_SERVER__LIMITS__PER_IP_BURST
  value: {{ .Values.server.limits.perIpBurst | quote }}
- name: EPHPM_SERVER__FILE_CACHE__ENABLED
  value: {{ .Values.server.fileCache.enabled | quote }}
- name: EPHPM_SERVER__FILE_CACHE__MAX_ENTRIES
  value: {{ .Values.server.fileCache.maxEntries | quote }}
- name: EPHPM_SERVER__FILE_CACHE__VALID_SECS
  value: {{ .Values.server.fileCache.validSecs | quote }}
- name: EPHPM_SERVER__FILE_CACHE__INACTIVE_SECS
  value: {{ .Values.server.fileCache.inactiveSecs | quote }}
- name: EPHPM_SERVER__FILE_CACHE__INLINE_THRESHOLD
  value: {{ .Values.server.fileCache.inlineThreshold | quote }}
- name: EPHPM_SERVER__FILE_CACHE__PRECOMPRESS
  value: {{ .Values.server.fileCache.precompress | quote }}
{{- if .Values.server.tls.enabled }}
{{- if .Values.server.tls.acme.domains }}
- name: EPHPM_SERVER__TLS__DOMAINS
  value: {{ .Values.server.tls.acme.domains | toJson | quote }}
{{- if .Values.server.tls.acme.email }}
- name: EPHPM_SERVER__TLS__EMAIL
  value: {{ .Values.server.tls.acme.email | quote }}
{{- end }}
- name: EPHPM_SERVER__TLS__CACHE_DIR
  value: {{ .Values.server.tls.acme.cacheDir | quote }}
- name: EPHPM_SERVER__TLS__STAGING
  value: {{ .Values.server.tls.acme.staging | quote }}
{{- else }}
- name: EPHPM_SERVER__TLS__CERT
  value: {{ .Values.server.tls.cert | quote }}
- name: EPHPM_SERVER__TLS__KEY
  value: {{ .Values.server.tls.key | quote }}
{{- end }}
{{- if .Values.server.tls.tlsListen }}
- name: EPHPM_SERVER__TLS__LISTEN
  value: {{ .Values.server.tls.tlsListen | quote }}
{{- end }}
- name: EPHPM_SERVER__TLS__REDIRECT_HTTP
  value: {{ .Values.server.tls.redirectHttp | quote }}
{{- end }}
- name: EPHPM_PHP__MAX_EXECUTION_TIME
  value: {{ .Values.php.maxExecutionTime | quote }}
- name: EPHPM_PHP__MEMORY_LIMIT
  value: {{ .Values.php.memoryLimit | quote }}
- name: EPHPM_PHP__WORKERS
  value: {{ .Values.php.workers | quote }}
{{- if .Values.php.iniFile }}
- name: EPHPM_PHP__INI_FILE
  value: {{ .Values.php.iniFile | quote }}
{{- end }}
{{- if .Values.db.mysql.enabled }}
{{- if .Values.db.mysql.existingSecret }}
- name: EPHPM_DB__MYSQL__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.db.mysql.existingSecret }}
      key: url
{{- else if .Values.db.mysql.url }}
- name: EPHPM_DB__MYSQL__URL
  value: {{ .Values.db.mysql.url | quote }}
{{- end }}
- name: EPHPM_DB__MYSQL__LISTEN
  value: {{ .Values.db.mysql.listen | quote }}
- name: EPHPM_DB__MYSQL__MIN_CONNECTIONS
  value: {{ .Values.db.mysql.minConnections | quote }}
- name: EPHPM_DB__MYSQL__MAX_CONNECTIONS
  value: {{ .Values.db.mysql.maxConnections | quote }}
- name: EPHPM_DB__MYSQL__IDLE_TIMEOUT
  value: {{ .Values.db.mysql.idleTimeout | quote }}
- name: EPHPM_DB__MYSQL__MAX_LIFETIME
  value: {{ .Values.db.mysql.maxLifetime | quote }}
- name: EPHPM_DB__MYSQL__POOL_TIMEOUT
  value: {{ .Values.db.mysql.poolTimeout | quote }}
- name: EPHPM_DB__MYSQL__HEALTH_CHECK_INTERVAL
  value: {{ .Values.db.mysql.healthCheckInterval | quote }}
- name: EPHPM_DB__MYSQL__INJECT_ENV
  value: {{ .Values.db.mysql.injectEnv | quote }}
- name: EPHPM_DB__MYSQL__RESET_STRATEGY
  value: {{ .Values.db.mysql.resetStrategy | quote }}
{{- end }}
{{- if .Values.db.postgres.enabled }}
{{- if .Values.db.postgres.existingSecret }}
- name: EPHPM_DB__POSTGRES__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.db.postgres.existingSecret }}
      key: url
{{- else if .Values.db.postgres.url }}
- name: EPHPM_DB__POSTGRES__URL
  value: {{ .Values.db.postgres.url | quote }}
{{- end }}
- name: EPHPM_DB__POSTGRES__LISTEN
  value: {{ .Values.db.postgres.listen | quote }}
- name: EPHPM_DB__POSTGRES__MIN_CONNECTIONS
  value: {{ .Values.db.postgres.minConnections | quote }}
- name: EPHPM_DB__POSTGRES__MAX_CONNECTIONS
  value: {{ .Values.db.postgres.maxConnections | quote }}
- name: EPHPM_DB__POSTGRES__IDLE_TIMEOUT
  value: {{ .Values.db.postgres.idleTimeout | quote }}
- name: EPHPM_DB__POSTGRES__MAX_LIFETIME
  value: {{ .Values.db.postgres.maxLifetime | quote }}
- name: EPHPM_DB__POSTGRES__POOL_TIMEOUT
  value: {{ .Values.db.postgres.poolTimeout | quote }}
- name: EPHPM_DB__POSTGRES__HEALTH_CHECK_INTERVAL
  value: {{ .Values.db.postgres.healthCheckInterval | quote }}
- name: EPHPM_DB__POSTGRES__INJECT_ENV
  value: {{ .Values.db.postgres.injectEnv | quote }}
- name: EPHPM_DB__POSTGRES__RESET_STRATEGY
  value: {{ .Values.db.postgres.resetStrategy | quote }}
{{- end }}
{{- if .Values.db.tds.enabled }}
{{- if .Values.db.tds.existingSecret }}
- name: EPHPM_DB__TDS__URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.db.tds.existingSecret }}
      key: url
{{- else if .Values.db.tds.url }}
- name: EPHPM_DB__TDS__URL
  value: {{ .Values.db.tds.url | quote }}
{{- end }}
- name: EPHPM_DB__TDS__LISTEN
  value: {{ .Values.db.tds.listen | quote }}
- name: EPHPM_DB__TDS__MIN_CONNECTIONS
  value: {{ .Values.db.tds.minConnections | quote }}
- name: EPHPM_DB__TDS__MAX_CONNECTIONS
  value: {{ .Values.db.tds.maxConnections | quote }}
- name: EPHPM_DB__TDS__INJECT_ENV
  value: {{ .Values.db.tds.injectEnv | quote }}
- name: EPHPM_DB__TDS__RESET_STRATEGY
  value: {{ .Values.db.tds.resetStrategy | quote }}
{{- end }}
{{- if .Values.db.sqlite.enabled }}
- name: EPHPM_DB__SQLITE__PATH
  value: {{ .Values.db.sqlite.path | quote }}
- name: EPHPM_DB__SQLITE__PROXY__MYSQL_LISTEN
  value: {{ .Values.db.sqlite.proxy.mysqlListen | quote }}
{{- if .Values.db.sqlite.proxy.hranaListen }}
- name: EPHPM_DB__SQLITE__PROXY__HRANA_LISTEN
  value: {{ .Values.db.sqlite.proxy.hranaListen | quote }}
{{- end }}
{{- if .Values.db.sqlite.proxy.postgresListen }}
- name: EPHPM_DB__SQLITE__PROXY__POSTGRES_LISTEN
  value: {{ .Values.db.sqlite.proxy.postgresListen | quote }}
{{- end }}
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
- name: EPHPM_DB__READ_WRITE_SPLIT__ENABLED
  value: {{ .Values.db.readWriteSplit.enabled | quote }}
- name: EPHPM_DB__READ_WRITE_SPLIT__STRATEGY
  value: {{ .Values.db.readWriteSplit.strategy | quote }}
- name: EPHPM_DB__READ_WRITE_SPLIT__STICKY_DURATION
  value: {{ .Values.db.readWriteSplit.stickyDuration | quote }}
- name: EPHPM_DB__READ_WRITE_SPLIT__MAX_REPLICA_LAG
  value: {{ .Values.db.readWriteSplit.maxReplicaLag | quote }}
- name: EPHPM_DB__ANALYSIS__QUERY_STATS
  value: {{ .Values.db.analysis.queryStats | quote }}
- name: EPHPM_DB__ANALYSIS__SLOW_QUERY_THRESHOLD
  value: {{ .Values.db.analysis.slowQueryThreshold | quote }}
- name: EPHPM_DB__ANALYSIS__DIGEST_STORE_MAX_ENTRIES
  value: {{ .Values.db.analysis.digestStoreMaxEntries | quote }}
- name: EPHPM_KV__MEMORY_LIMIT
  value: {{ .Values.kv.memoryLimit | quote }}
- name: EPHPM_KV__EVICTION_POLICY
  value: {{ .Values.kv.evictionPolicy | quote }}
- name: EPHPM_KV__COMPRESSION
  value: {{ .Values.kv.compression | quote }}
- name: EPHPM_KV__COMPRESSION_LEVEL
  value: {{ .Values.kv.compressionLevel | quote }}
- name: EPHPM_KV__COMPRESSION_MIN_SIZE
  value: {{ .Values.kv.compressionMinSize | quote }}
{{- if .Values.kv.secret }}
- name: EPHPM_KV__SECRET
  value: {{ .Values.kv.secret | quote }}
{{- end }}
- name: EPHPM_KV__REDIS_COMPAT__ENABLED
  value: {{ .Values.kv.redisCompat.enabled | quote }}
- name: EPHPM_KV__REDIS_COMPAT__LISTEN
  value: {{ .Values.kv.redisCompat.listen | quote }}
{{- if .Values.kv.redisCompat.password }}
- name: EPHPM_KV__REDIS_COMPAT__PASSWORD
  value: {{ .Values.kv.redisCompat.password | quote }}
{{- end }}
{{- if .Values.cluster.enabled }}
- name: EPHPM_CLUSTER__ENABLED
  value: "true"
- name: EPHPM_CLUSTER__BIND
  value: {{ .Values.cluster.bind | quote }}
- name: EPHPM_CLUSTER__JOIN
  value: {{ include "ephpm.clusterJoinJson" . | quote }}
- name: EPHPM_CLUSTER__CLUSTER_ID
  value: {{ .Values.cluster.clusterId | quote }}
{{- if .Values.cluster.nodeId }}
- name: EPHPM_CLUSTER__NODE_ID
  value: {{ .Values.cluster.nodeId | quote }}
{{- end }}
{{- if .Values.cluster.existingSecret }}
- name: EPHPM_CLUSTER__SECRET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.cluster.existingSecret }}
      key: gossipSecret
{{- else if .Values.cluster.secret }}
- name: EPHPM_CLUSTER__SECRET
  value: {{ .Values.cluster.secret | quote }}
{{- end }}
- name: EPHPM_CLUSTER__KV__SMALL_KEY_THRESHOLD
  value: {{ .Values.cluster.kv.smallKeyThreshold | quote }}
- name: EPHPM_CLUSTER__KV__REPLICATION_FACTOR
  value: {{ .Values.cluster.kv.replicationFactor | quote }}
- name: EPHPM_CLUSTER__KV__REPLICATION_MODE
  value: {{ .Values.cluster.kv.replicationMode | quote }}
- name: EPHPM_CLUSTER__KV__HOT_KEY_CACHE
  value: {{ .Values.cluster.kv.hotKeyCache | quote }}
- name: EPHPM_CLUSTER__KV__HOT_KEY_THRESHOLD
  value: {{ .Values.cluster.kv.hotKeyThreshold | quote }}
- name: EPHPM_CLUSTER__KV__HOT_KEY_WINDOW_SECS
  value: {{ .Values.cluster.kv.hotKeyWindowSecs | quote }}
- name: EPHPM_CLUSTER__KV__HOT_KEY_LOCAL_TTL_SECS
  value: {{ .Values.cluster.kv.hotKeyLocalTtlSecs | quote }}
- name: EPHPM_CLUSTER__KV__HOT_KEY_MAX_MEMORY
  value: {{ .Values.cluster.kv.hotKeyMaxMemory | quote }}
- name: EPHPM_CLUSTER__KV__DATA_PORT
  value: {{ .Values.cluster.kv.dataPort | quote }}
{{- end }}
{{- with .Values.extraEnv }}
{{- toYaml . | nindent 0 }}
{{- end }}
{{- end }}
