import { useEffect, useRef, useState } from 'react'
import { absoluteDownloadUrl, createTask, getTask, type ResolveTask } from './api'
import './App.css'

function App() {
  const [shareText, setShareText] = useState('')
  const [task, setTask] = useState<ResolveTask | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const pollRef = useRef<number | null>(null)

  useEffect(() => () => {
    if (pollRef.current) window.clearTimeout(pollRef.current)
  }, [])

  async function submit() {
    if (!shareText.trim() || busy) return
    setBusy(true)
    setError('')
    setTask(null)
    try {
      const created = await createTask(shareText)
      const poll = async () => {
        const next = await getTask(created.statusUrl)
        setTask(next)
        if (next.status === 'queued' || next.status === 'processing') {
          pollRef.current = window.setTimeout(poll, 900)
        } else {
          setBusy(false)
          if (next.status === 'failed') setError(next.error || '任务处理失败')
        }
      }
      await poll()
    } catch (requestError) {
      setBusy(false)
      setError(requestError instanceof Error ? requestError.message : '无法连接解析服务')
    }
  }

  function reset() {
    setShareText('')
    setTask(null)
    setError('')
    setBusy(false)
  }

  const progress = task?.progress ?? 0
  const completed = task?.status === 'completed'
  const files = task?.files ?? []
  const isGallery = files.some((file) => file.kind === 'image')

  return (
    <main className="shell">
      <header className="topbar">
        <a className="brand" href="/" aria-label="QSY Tool 首页"><span className="brand-mark">Q</span><span>QSY Tool</span></a>
        <span className="service-pill"><i />服务器解析</span>
      </header>

      <section className="hero">
        <p className="eyebrow">SHARE LINK RESOLVER</p>
        <h1>把分享链接，<em>变成可下载文件</em></h1>
        <p className="subtitle">粘贴抖音分享文案，解析与下载都在服务器后台完成。<br />无需安装额外工具，也不用等待页面停留。</p>

        <div className={`work-card ${task ? 'has-task' : ''}`}>
          {!task && <>
            <label htmlFor="share-input">分享文案</label>
            <textarea id="share-input" value={shareText} onChange={(event) => setShareText(event.target.value)} placeholder="粘贴包含抖音链接的分享文案…" disabled={busy} />
            <div className="card-footer"><span className="hint">支持完整分享文案或单独链接</span><button className="primary" onClick={submit} disabled={!shareText.trim() || busy}>开始解析 <span>→</span></button></div>
          </>}
          {task && <div className="task-view">
            <div className="task-head"><div><span className="task-label">任务状态</span><h2>{completed ? '文件已准备好' : task.status === 'failed' ? '处理未完成' : '正在后台处理'}</h2></div><span className={`status-dot ${task.status}`} /> </div>
            <p className="stage">{task.stage}</p>
            <div className="progress"><span style={{ width: `${progress}%` }} /></div>
            <div className="progress-meta"><span>{task.status === 'failed' ? '请检查链接后重试' : `${progress}%`}</span><span>{task.videoId ? `ID ${task.videoId}` : 'QSY SERVER'}</span></div>
            {completed && files.length > 0 && <div className={`file-list ${isGallery ? 'gallery-list' : ''}`}>
              {files.map((file) => <a className={`download ${file.kind === 'image' ? 'image-download' : ''}`} href={absoluteDownloadUrl(file.downloadUrl)} download key={file.downloadUrl}>
                <span className="download-icon">{file.kind === 'image' ? file.index + 1 : '↓'}</span>
                <span><strong>{file.kind === 'image' ? `下载图片 ${file.index + 1}` : '下载 MP4 文件'}</strong><small>{file.name}</small></span><b>↗</b>
              </a>)}
            </div>}
            {completed && files.length === 0 && task.downloadUrl && <a className="download" href={absoluteDownloadUrl(task.downloadUrl)} download><span className="download-icon">↓</span><span><strong>下载 MP4 文件</strong><small>文件已保存到服务器，点击开始下载</small></span><b>↗</b></a>}
            {task.status === 'failed' && <p className="error-text">{task.error}</p>}
            <button className="text-button" onClick={reset}>{completed ? '解析新的链接' : '返回重新输入'}</button>
          </div>}
        </div>
        {error && !task && <p className="error-text standalone">{error}</p>}
      </section>

      <footer className="footnote"><span>安全 · 快速 · 不保留分享原文</span><span>QSY TOOL <b>•</b> v2.0</span></footer>
    </main>
  )
}

export default App
