{{- define "ephpm.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "ephpm.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "ephpm.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "ephpm.labels" -}}
helm.sh/chart: {{ include "ephpm.chart" . }}
{{ include "ephpm.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "ephpm.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ephpm.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "ephpm.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ephpm.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "ephpm.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end }}

{{- define "ephpm.headlessServiceName" -}}
{{- printf "%s-headless" (include "ephpm.fullname" .) }}
{{- end }}

{{/*
Build the EPHPM_CLUSTER__JOIN JSON array from the StatefulSet pod DNS names.
Matches the pattern in k8s/base/ephpm-cluster.yaml:
  ["pod-0.headless-svc:7946","pod-1.headless-svc:7946",...]
If cluster.join is set explicitly, use that as-is (JSON encoded).
*/}}
{{- define "ephpm.clusterJoinJson" -}}
{{- if .Values.cluster.join }}
{{- .Values.cluster.join | toJson }}
{{- else }}
{{- $peers := list }}
{{- $fullname := include "ephpm.fullname" . }}
{{- $headless := include "ephpm.headlessServiceName" . }}
{{- range $i, $_ := until (int .Values.replicaCount) }}
{{- $peers = append $peers (printf "%s-%d.%s:7946" $fullname $i $headless) }}
{{- end }}
{{- $peers | toJson }}
{{- end }}
{{- end }}

{{/*
True when the workload must be a StatefulSet (clustering or sqlite persistence).
*/}}
{{- define "ephpm.isStatefulSet" -}}
{{- if or .Values.cluster.enabled .Values.db.sqlite.enabled -}}true{{- end }}
{{- end }}
