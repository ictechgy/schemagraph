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
import time

# CI 러너의 headless Chrome 기동 시간은 크게 흔들린다. 2026-10-19 main CI에서 두 번 띄우는 데
# 1.8~18.9초가 걸렸고, 한 번 띄우는 데 30초를 넘긴 실행이 두 번 연속 나왔다. 기동 지연을
# 결함으로 오판하지 않도록 넉넉하게 기다리고, 실제 소요 시간은 결과 줄에 남긴다.
BROWSER_TIMEOUT_SECONDS = 120


def browser_environment(work):
    """D-Bus 접속이 곧바로 실패하도록 버스 주소를 없는 소켓으로 바꾼 환경을 돌려준다.

    이 검사는 D-Bus가 필요 없다. 그런데 러너의 버스가 응답하지 않으면 Chrome은 호출마다
    기본 대기(25초)를 다 쓴 뒤에야 진행한다. 실패한 실행의 D-Bus 오류가 기동 약 26초 뒤에
    찍힌 것이 그 정황이다. 없는 경로를 주면 연결 시도가 즉시 실패하고 검사는 그대로 진행된다.
    """
    environment = dict(os.environ)
    missing = 'unix:path='+str(work/'no-dbus-socket')
    environment['DBUS_SESSION_BUS_ADDRESS'] = missing
    environment['DBUS_SYSTEM_BUS_ADDRESS'] = missing
    return environment


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
    elapsed = []
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
            started = time.monotonic()
            process = subprocess.Popen(command+[html.as_uri()],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True,env=browser_environment(work))
            try:
                stdout, stderr = process.communicate(timeout=BROWSER_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
                raise RuntimeError(f'headless Chrome timed out after {BROWSER_TIMEOUT_SECONDS}s (rich={rich}); check runner load or pass --browser: '+stderr[-1500:])
            elapsed.append(time.monotonic()-started)
            if process.returncode or 'data-qa-result="pass"' not in stdout:
                error = re.search(r'data-qa-error="([^"]+)"',stdout)
                raise RuntimeError(f'HTML browser verification failed (rich={rich}): '+(error[1] if error else 'browser or runtime failure'))
    print('offline HTML browser interaction: ok (browser seconds: '+', '.join(f'{seconds:.1f}' for seconds in elapsed)+')')


if __name__=='__main__':
    main()
