import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { createLocalDevProxy } from '../vite.local-proxy'

describe('createLocalDevProxy', () => {
  it('is disabled when no native runtime configuration is present', () => {
    expect(createLocalDevProxy({})).toBeUndefined()
  })

  it('reads the access token only into the Vite server proxy', () => {
    const root = mkdtempSync(join(tmpdir(), 'agent-runtime-vite-proxy-'))
    try {
      const tokenPath = join(root, 'token')
      writeFileSync(tokenPath, 'signed-local-token\n', { mode: 0o600 })

      const proxy = createLocalDevProxy({
        AGENT_RUNTIME_CONTROL_API: 'http://127.0.0.1:8080',
        AGENT_RUNTIME_LOCAL_ACCESS_TOKEN_FILE: tokenPath,
      })

      expect(proxy).toEqual({
        '/v1': {
          target: 'http://127.0.0.1:8080',
          changeOrigin: false,
          headers: { Authorization: 'Bearer signed-local-token' },
        },
      })
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  it('rejects partial configuration and non-loopback token forwarding', () => {
    expect(() => createLocalDevProxy({
      AGENT_RUNTIME_CONTROL_API: 'http://127.0.0.1:8080',
    })).toThrow(/must be configured together/)

    expect(() => createLocalDevProxy({
      AGENT_RUNTIME_CONTROL_API: 'https://api.example.com',
      AGENT_RUNTIME_LOCAL_ACCESS_TOKEN_FILE: '/tmp/token',
    })).toThrow(/loopback/)
  })
})
