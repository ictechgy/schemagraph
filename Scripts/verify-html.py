#!/usr/bin/env python3
"""실제 headless Chrome에서 오프라인 HTML 검색·선택·빈 선택 필드를 검사한다."""
import argparse
import json
from pathlib import Path
import re
import shutil
import os
import signal
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', type=Path, required=True)
    parser.add_argument('--browser')
    parser.add_argument('--screenshot', type=Path)
    args = parser.parse_args()
    browser = args.browser or shutil.which('google-chrome') or shutil.which('chromium')
    if not browser:
        candidate = Path('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome')
        browser = str(candidate) if candidate.is_file() else None
    if not browser:
        parser.error('headless Chrome is required; pass --browser')
    with tempfile.TemporaryDirectory(prefix='schemagraph-html-') as directory:
        work = Path(directory)
        for rich in (False, True):
            vertices = [{'id':'main.orders','kind':'table','level':'object','name':'orders','schema':'main'}]
            graph = {'version':2,'vertices':vertices,'edges':[]}
            search = 'orders'
            if rich:
                vertices.append({'id':'main.query_01','kind':'query','level':'object','name':'query_01','schema':'main'})
                graph['edges'] = [{'from':'main.query_01','to':'main.orders','kind':'reads'}]
                graph['analysis'] = [{'id':'main.query_01','scope':'object-dependencies','state':'complete','diagnostics':[],'source':'reports/orders.sql'}]
                search = 'reports/orders.sql'
            catalog = work/'input.json'
            catalog.write_text(json.dumps(graph))
            output = subprocess.run([str(args.engine.resolve()),'graph','--graph',str(catalog),'--format','html'],capture_output=True,text=True,check=True).stdout
            checks = """
<script>
try {
  const input=document.getElementById('search');
  input.value=SEARCH; input.dispatchEvent(new Event('input'));
  const button=document.querySelector('#results button');
  if (!button) throw new Error('search has no result');
  button.click();
  if (document.getElementById('details').hidden) throw new Error('selection stayed hidden');
  if (RICH && !document.getElementById('analysis').textContent.includes(SEARCH)) throw new Error('source path is missing');
  if (RICH && document.querySelectorAll('#neighbors .edge-card').length!==1) throw new Error('incident edge missing');
  document.body.dataset.qaResult='pass';
} catch(error) { document.body.dataset.qaResult='fail'; document.body.dataset.qaError=error.message; }
</script>
""".replace('SEARCH',json.dumps(search)).replace('RICH',str(rich).lower())
            html = work/'report.html'
            html.write_text(output + checks)
            command = [browser,'--headless','--no-sandbox','--disable-gpu','--disable-background-networking','--disable-default-apps','--disable-extensions','--disable-sync','--password-store=basic','--use-mock-keychain','--no-first-run','--no-default-browser-check','--window-size=1280,1000','--virtual-time-budget=1000','--dump-dom',f'--user-data-dir={work / ("profile-rich" if rich else "profile-empty")}']
            if rich and args.screenshot:
                command.append('--screenshot='+str(args.screenshot.resolve()))
            process = subprocess.Popen(command+[html.as_uri()],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
                raise RuntimeError('headless Chrome timed out: '+stderr[-1500:])
            if process.returncode or 'data-qa-result="pass"' not in stdout:
                error = re.search(r'data-qa-error="([^"]+)"',stdout)
                raise RuntimeError(f'HTML browser verification failed (rich={rich}): '+(error[1] if error else 'browser or runtime failure'))
    print('offline HTML browser interaction: ok')


if __name__=='__main__':
    main()
