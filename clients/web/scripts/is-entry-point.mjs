import { realpathSync } from 'node:fs'
import { pathToFileURL } from 'node:url'

// Whether the module whose `import.meta.url` is `moduleUrl` is the file node was started
// on, which is how a script that can also be imported tells a command line from an import.
//
// Node resolves a module's URL through symlinks, but `process.argv[1]` is the path as it
// was typed, so comparing the two directly is false for a checkout reached through a
// symlink (a `~/ma` alias for the working copy is one) and the command then ran nothing,
// printed nothing and exited 0. The entry path is resolved first.
export const isEntryPoint = (moduleUrl, entry = process.argv[1]) => {
  if (!entry) return false
  try {
    return moduleUrl === pathToFileURL(realpathSync(entry)).href
  } catch {
    return false
  }
}
