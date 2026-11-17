// Summarise cache-tests results by test kind. argv: <cache-tests dir> <result.json>...
import fs from 'node:fs'
const [ctDir, ...files] = process.argv.slice(1)
const suites = (await import(`${ctDir}/tests/index.mjs`)).default
const kind = {}
for (const s of suites) for (const t of s.tests) kind[t.id] = t.kind || 'required'
for (const f of files) {
  const r = JSON.parse(fs.readFileSync(f))
  const c = {}
  for (const [id, v] of Object.entries(r)) {
    if (id.startsWith('_') || !(id in kind)) continue
    const k = kind[id]
    c[k] ??= { pass: 0, fail: 0, other: 0 }
    if (v === true) c[k].pass++
    else if (Array.isArray(v) && (v[0] === 'Assertion' || v[0] === 'AssertionError' || v[0] === 'Failure')) c[k].fail++
    else c[k].other++
  }
  console.log(f.split('/').pop(), JSON.stringify(c))
}
