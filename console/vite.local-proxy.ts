import { readFileSync, statSync } from 'node:fs'
import type { ProxyOptions } from 'vite'

interface NativeProxyEnvironment {
  [key: string]: string | undefined
  AGENT_RUNTIME_CONTROL_API?: string
  AGENT_RUNTIME_LOCAL_ACCESS_TOKEN_FILE?: string
}

export function createLocalDevProxy(environment: NativeProxyEnvironment): Record<string, ProxyOptions> | undefined {
  const targetValue = environment.AGENT_RUNTIME_CONTROL_API
  const tokenPath = environment.AGENT_RUNTIME_LOCAL_ACCESS_TOKEN_FILE
  if (targetValue === undefined && tokenPath === undefined) return undefined
  if (!targetValue || !tokenPath) {
    throw new Error('native control API and access token file must be configured together')
  }

  const target = new URL(targetValue)
  if (target.protocol !== 'http:' || !['127.0.0.1', 'localhost'].includes(target.hostname)) {
    throw new Error('native development credentials may only be forwarded to a loopback HTTP API')
  }
  if (target.username || target.password || target.search || target.hash || target.pathname !== '/') {
    throw new Error('native control API must be an origin without credentials, path, query, or fragment')
  }

  const tokenMode = statSync(tokenPath).mode & 0o077
  if (tokenMode !== 0) throw new Error('native access token file permissions are too broad')
  const token = readFileSync(tokenPath, 'utf8').trim()
  if (!/^[A-Za-z0-9._-]+$/.test(token)) throw new Error('native access token is invalid')

  return {
    '/v1': {
      target: target.origin,
      changeOrigin: false,
      headers: { Authorization: `Bearer ${token}` },
    },
  }
}
