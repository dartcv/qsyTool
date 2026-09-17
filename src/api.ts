export type TaskStatus = 'queued' | 'processing' | 'completed' | 'failed'

export interface TaskFile {
  kind: 'video' | 'image'
  name: string
  downloadUrl: string
  index: number
}

export interface ResolveTask {
  id: string
  status: TaskStatus
  stage: string
  progress: number | null
  title: string | null
  videoId: string | null
  downloadUrl: string | null
  files: TaskFile[]
  error: string | null
}

interface CreateTaskResponse {
  taskId: string
  statusUrl: string
}

interface ApiErrorBody {
  error?: string
}

const API_BASE = (import.meta.env.VITE_API_BASE_URL as string | undefined)?.replace(/\/$/, '') ?? ''

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${path}`, init)
  const body = (await response.json().catch(() => ({}))) as T & ApiErrorBody
  if (!response.ok) {
    throw new Error(body.error || `请求失败（${response.status}）`)
  }
  return body
}

export async function createTask(shareText: string): Promise<CreateTaskResponse> {
  return request<CreateTaskResponse>('/api/v1/tasks', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ shareText }),
  })
}

export async function getTask(statusUrl: string): Promise<ResolveTask> {
  return request<ResolveTask>(statusUrl)
}

export function absoluteDownloadUrl(path: string): string {
  return new URL(`${API_BASE}${path}`, window.location.href).href
}
