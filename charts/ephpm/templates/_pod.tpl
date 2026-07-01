{{/*
Shared pod template spec (metadata + spec) for the Deployment and StatefulSet.
Include with `nindent 4` directly under a `template:` key.
*/}}
{{- define "ephpm.podTemplateSpec" -}}
metadata:
  labels:
    {{- include "ephpm.labels" . | nindent 4 }}
    {{- with .Values.podLabels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  annotations:
    checksum/config: {{ include (print $.Template.BasePath "/configmap.yaml") . | sha256sum }}
    {{- with .Values.podAnnotations }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  {{- with .Values.imagePullSecrets }}
  imagePullSecrets:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  serviceAccountName: {{ include "ephpm.serviceAccountName" . }}
  {{- with .Values.podSecurityContext }}
  securityContext:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  containers:
    - name: {{ .Chart.Name }}
      image: {{ include "ephpm.image" . | quote }}
      imagePullPolicy: {{ .Values.image.pullPolicy }}
      {{- with .Values.command }}
      command:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.args }}
      args:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      ports:
        - name: http
          containerPort: {{ .Values.containerPort }}
          protocol: TCP
        {{- if .Values.tls.enabled }}
        - name: https
          containerPort: {{ (splitList ":" .Values.tls.listen) | last | int }}
          protocol: TCP
        {{- end }}
        {{- if .Values.cluster.enabled }}
        - name: gossip-tcp
          containerPort: {{ .Values.cluster.gossipPort }}
          protocol: TCP
        - name: gossip-udp
          containerPort: {{ .Values.cluster.gossipPort }}
          protocol: UDP
        - name: kv-data
          containerPort: {{ .Values.cluster.kv.dataPort }}
          protocol: TCP
        {{- end }}
      env:
        {{- include "ephpm.configEnv" . | nindent 8 }}
        {{- with .Values.extraEnv }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- if or .Values.existingSecret .Values.extraEnvFrom }}
      envFrom:
        {{- if .Values.existingSecret }}
        - secretRef:
            name: {{ .Values.existingSecret }}
        {{- end }}
        {{- with .Values.extraEnvFrom }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- end }}
      {{- with .Values.livenessProbe }}
      livenessProbe:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.readinessProbe }}
      readinessProbe:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.startupProbe }}
      startupProbe:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.resources }}
      resources:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.securityContext }}
      securityContext:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      volumeMounts:
        - name: config
          mountPath: /etc/ephpm
          readOnly: true
        - name: tmp
          mountPath: /tmp
        {{- if .Values.db.sqlite.enabled }}
        - name: data
          mountPath: /var/lib/ephpm/sqlite
        {{- end }}
        {{- if and .Values.persistence.enabled (not (include "ephpm.isStatefulSet" .)) }}
        - name: www
          mountPath: {{ .Values.persistence.mountPath }}
        {{- end }}
        {{- if and .Values.tls.enabled (eq .Values.tls.mode "manual") }}
        - name: tls
          mountPath: {{ .Values.tls.mountPath }}
          readOnly: true
        {{- end }}
        {{- if and .Values.tls.enabled (eq .Values.tls.mode "acme") }}
        - name: acme-cache
          mountPath: {{ .Values.tls.acme.cacheDir }}
        {{- end }}
        {{- with .Values.extraVolumeMounts }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
  volumes:
    - name: config
      configMap:
        name: {{ include "ephpm.fullname" . }}
    - name: tmp
      emptyDir: {}
    {{- if and .Values.persistence.enabled (not (include "ephpm.isStatefulSet" .)) }}
    - name: www
      persistentVolumeClaim:
        claimName: {{ .Values.persistence.existingClaim | default (printf "%s-www" (include "ephpm.fullname" .)) }}
    {{- end }}
    {{- if and .Values.tls.enabled (eq .Values.tls.mode "manual") }}
    - name: tls
      secret:
        secretName: {{ .Values.tls.secretName }}
    {{- end }}
    {{- if and .Values.tls.enabled (eq .Values.tls.mode "acme") }}
    - name: acme-cache
      emptyDir: {}
    {{- end }}
    {{- with .Values.extraVolumes }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.nodeSelector }}
  nodeSelector:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.affinity }}
  affinity:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.tolerations }}
  tolerations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.topologySpreadConstraints }}
  topologySpreadConstraints:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end }}
