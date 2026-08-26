/** Write `text` to the system clipboard. False when the API is missing or
 *  the write is rejected (permissions, insecure context). */
export async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard?.writeText === undefined) {
    return false
  }
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}
