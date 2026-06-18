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
{{- printf "%s-headless" (include "ephpm.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
The workload is a StatefulSet when clustering OR sqlite is enabled (both need
stable identity / storage); otherwise a Deployment.
*/}}
{{- define "ephpm.isStatefulSet" -}}
{{- if or .Values.cluster.enabled .Values.db.sqlite.enabled -}}true{{- end -}}
{{- end }}

{{/*
Replica count for the rendered workload.
*/}}
{{- define "ephpm.replicas" -}}
{{- if .Values.cluster.enabled -}}
{{- .Values.cluster.replicas -}}
{{- else -}}
{{- .Values.replicaCount -}}
{{- end -}}
{{- end }}

{{/*
EPHPM_CLUSTER__JOIN seed list as a JSON array string. Explicit cluster.join
wins; otherwise derive per-pod FQDNs from the headless Service.
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
