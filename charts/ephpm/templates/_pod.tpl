{{/*
Shared pod template spec (metadata + spec) for both the Deployment and the
StatefulSet. Include with `nindent 4` directly under a `template:` key.
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
      image: "{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}"
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
        {{- if .Values.cluster.enabled }}
        - name: gossip-tcp
          containerPort: {{ .Values.cluster.gossipPort }}
          protocol: TCP
        - name: gossip-udp
          containerPort: {{ .Values.cluster.gossipPort }}
          protocol: UDP
        {{- end }}
      env:
        - name: EPHPM_SERVER__METRICS__ENABLED
          value: {{ .Values.metrics.enabled | quote }}
        - name: EPHPM_SERVER__METRICS__PATH
          value: {{ .Values.metrics.path | quote }}
        {{- if .Values.cluster.enabled }}
        - name: EPHPM_CLUSTER__ENABLED
          value: "true"
        - name: EPHPM_CLUSTER__BIND
          value: "0.0.0.0:{{ .Values.cluster.gossipPort }}"
        - name: EPHPM_CLUSTER__CLUSTER_ID
          value: {{ .Values.cluster.clusterId | default (include "ephpm.fullname" .) | quote }}
        - name: EPHPM_CLUSTER__JOIN
          value: {{ include "ephpm.gossipJoin" . | quote }}
        {{- if .Values.cluster.secret }}
        - name: EPHPM_CLUSTER__SECRET
          valueFrom:
            secretKeyRef:
              name: {{ include "ephpm.fullname" . }}-cluster
              key: gossip-secret
        {{- end }}
        {{- end }}
        {{- with .Values.extraEnv }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- if or .Values.secretEnv .Values.existingSecret .Values.extraEnvFrom }}
      envFrom:
        {{- if .Values.secretEnv }}
        - secretRef:
            name: {{ include "ephpm.fullname" . }}-env
        {{- end }}
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
        {{- with .Values.extraVolumeMounts }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
  volumes:
    - name: config
      configMap:
        name: {{ include "ephpm.fullname" . }}
    - name: tmp
      emptyDir: {}
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
