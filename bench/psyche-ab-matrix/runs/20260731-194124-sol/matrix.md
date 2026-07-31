# Psyche × OCAP A/B matrix — gpt-5.6-sol

Endpoint `https://api.openai.com` · each cell = pass-rate over the task set + avg tokens/wall per task.

| posture \ OCAP | off | on |
|---|---|---|
| **baseline** | 3/3 pass · 4335 tok · 4s | 3/3 pass · 4331 tok · 5s |
| **tenacity** | 3/3 pass · 4330 tok · 5s | 3/3 pass · 4326 tok · 4s |
| **crew** | 3/3 pass · 4331 tok · 4s | 3/3 pass · 4317 tok · 4s |
| **obsessive** | 3/3 pass · 4329 tok · 5s | 3/3 pass · 4329 tok · 5s |

## Per-cell task detail

| posture | ocap | task | verify | status | tools | writes | tokens | wall |
|---|---|---|---|---|---|---|---|---|
| baseline | off | edit-version | pass | completed | 1 | 1 | 4341 | 2.6 |
| baseline | off | fix-typo | pass | completed | 2 | 1 | 4371 | 5.6 |
| baseline | off | write-greeting | pass | completed | 1 | 1 | 4293 | 3.2 |
| baseline | on | edit-version | pass | completed | 1 | 1 | 4333 | 3.3 |
| baseline | on | fix-typo | pass | completed | 2 | 1 | 4375 | 3.9 |
| baseline | on | write-greeting | pass | completed | 1 | 1 | 4287 | 7.5 |
| tenacity | off | edit-version | pass | completed | 1 | 1 | 4329 | 5.4 |
| tenacity | off | fix-typo | pass | completed | 2 | 1 | 4371 | 6.2 |
| tenacity | off | write-greeting | pass | completed | 1 | 1 | 4291 | 2.8 |
| tenacity | on | edit-version | pass | completed | 1 | 1 | 4323 | 2.6 |
| tenacity | on | fix-typo | pass | completed | 2 | 1 | 4365 | 4.6 |
| tenacity | on | write-greeting | pass | completed | 1 | 1 | 4291 | 3.7 |
| crew | off | edit-version | pass | completed | 1 | 1 | 4335 | 2.4 |
| crew | off | fix-typo | pass | completed | 2 | 1 | 4379 | 4.2 |
| crew | off | write-greeting | pass | completed | 1 | 1 | 4281 | 4.4 |
| crew | on | edit-version | pass | completed | 1 | 1 | 4325 | 2.9 |
| crew | on | fix-typo | pass | completed | 2 | 1 | 4371 | 5.8 |
| crew | on | write-greeting | pass | completed | 1 | 1 | 4257 | 2.8 |
| obsessive | off | edit-version | pass | completed | 1 | 1 | 4325 | 3.5 |
| obsessive | off | fix-typo | pass | completed | 2 | 1 | 4367 | 5.3 |
| obsessive | off | write-greeting | pass | completed | 1 | 1 | 4297 | 7.0 |
| obsessive | on | edit-version | pass | completed | 1 | 1 | 4333 | 3.9 |
| obsessive | on | fix-typo | pass | completed | 2 | 1 | 4371 | 5.8 |
| obsessive | on | write-greeting | pass | completed | 1 | 1 | 4285 | 4.8 |
