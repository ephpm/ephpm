{{/*
Expand the name of the chart.
*/}}
{{- define "ephpm.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name.
*/}}
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

{{/*
Common labels.
*/}}
{{- define "ephpm.labels" -}}
helm.sh/chart: {{ include "ephpm.chart" . }}
{{ include "ephpm.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels (stable subset used by Services and workload selectors).
*/}}
{{- define "ephpm.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ephpm.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
ServiceAccount name.
*/}}
{{- define "ephpm.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ephpm.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Headless Service name (gossip DNS discovery in cluster mode).
*/}}
{{- define "ephpm.headlessServiceName" -}}
{{- printf "%s-headless" (include "ephpm.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Gossip seed list as a JSON array string for EPHPM_CLUSTER__JOIN.
Uses an explicit list if provided, otherwise derives stable per-pod FQDNs
from the headless Service for `cluster.replicas` pods.
*/}}
{{- define "ephpm.gossipJoin" -}}
{{- if .Values.cluster.join -}}
{{ .Values.cluster.join | toJson }}
{{- else -}}
{{- $full := include "ephpm.fullname" . -}}
{{- $hl := include "ephpm.headlessServiceName" . -}}
{{- $ns := .Release.Namespace -}}
{{- $dom := .Values.clusterDomain | default "cluster.local" -}}
{{- $port := int .Values.cluster.gossipPort -}}
{{- $seeds := list -}}
{{- range $i := until (int .Values.cluster.replicas) -}}
{{- $seeds = append $seeds (printf "%s-%d.%s.%s.svc.%s:%d" $full $i $hl $ns $dom $port) -}}
{{- end -}}
{{ $seeds | toJson }}
{{- end -}}
{{- end }}
