import json, os, platform, statistics, subprocess
from pathlib import Path
rows=[]
for files in [16, 1024, 10000]:
    for contexts in [1, 8]:
        samples={"base":[], "candidate":[]}
        iterations=max(20,100000//files)
        for round in range(10):
            order=["base","candidate"] if round%2==0 else ["candidate","base"]
            for build in order:
                ms=float(subprocess.check_output([f"/tmp/index-{build}",str(files),str(iterations),str(contexts)],text=True))
                samples[build].append(ms)
        rows.append(dict(files=files,contexts=contexts,iterations=iterations,samples_ms=samples,median_ms={k:statistics.median(v) for k,v in samples.items()}))
result=dict(platform=platform.platform(),cpu=Path('/proc/cpuinfo').read_text().split('model name')[1].split('\n')[0],cpus=os.cpu_count(),load=os.getloadavg(),rows=rows)
Path('/tmp/results.json').write_text(json.dumps(result,indent=2))
print(json.dumps(result,indent=2))
