import { createElement } from 'react'
import { renderToString } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import App from './App'

describe('App', () => {
  it('renders the local chess shell', () => {
    const html = renderToString(createElement(App))

    expect(html).toContain('Chess')
    expect(html).toContain('Local two-player chess')
    expect(html).toContain('Playable chess board')
  })
})
