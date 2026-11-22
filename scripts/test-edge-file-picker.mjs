// Local DevTools fixture for the ignored browser_upload_picker_first_show test.
import { readFile } from 'node:fs/promises';
import { setTimeout as wait } from 'node:timers/promises';

const [portFile, mode] = process.argv.slice(2);
const deadline = Date.now() + 10000;
const timeout = setTimeout(() => {
  console.error('Edge fixture timed out');
  process.exit(1);
}, 12000);

let page;
while (Date.now() < deadline) {
  try {
    const [port] = (await readFile(portFile, 'utf8')).split(/\r?\n/);
    const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    page = pages.find(p => p.url.startsWith('data:text/html,<input') && p.type === 'page');
    if (page) break;
  } catch {}
  await wait(50);
}
if (!page) throw new Error('Test page not found');
const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true });
  socket.addEventListener('error', reject, { once: true });
});
try {
  for (let id = 1; Date.now() < deadline; id++) {
    const result = new Promise(resolve => socket.addEventListener('message', event => resolve(JSON.parse(event.data)), { once: true }));
    const expression = mode === 'files'
      ? 'Array.from(document.querySelector("input").files, file => file.name)'
      : '(() => { const input = document.querySelector("input"); if (!input) return null; const at = Date.now(); input.click(); return at; })()';
    socket.send(JSON.stringify({ id, method: 'Runtime.evaluate', params: {
      expression, userGesture: mode === 'open', returnByValue: true,
    }}));
    const value = await result;
    if (value.error || value.result?.exceptionDetails) throw new Error(JSON.stringify(value));
    const output = value.result?.result?.value;
    if (output != null) { console.log(JSON.stringify(output)); process.exitCode = 0; break; }
    await wait(50);
  }
  if (process.exitCode !== 0) throw new Error('Test input did not load');
} finally { socket.close(); clearTimeout(timeout); }
