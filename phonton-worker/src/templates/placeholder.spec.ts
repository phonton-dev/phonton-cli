import { expect, test } from '@playwright/test'

test('playable chess app loads after the benchmark task', async ({ page }) => {
  await page.goto('/')

  await expect(page.getByRole('heading', { name: /^Chess$/i })).toBeVisible()
  await expect(page.getByRole('grid', { name: /playable chess board/i })).toBeVisible()
  await expect(page.getByRole('button', { name: /a1 rook/i })).toBeVisible()
  await expect(page.getByRole('button', { name: /reset/i })).toBeVisible()
  await expect(page.getByText(/White to move/i)).toBeVisible()
})
