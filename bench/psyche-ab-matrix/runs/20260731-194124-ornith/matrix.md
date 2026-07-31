# Psyche × OCAP A/B matrix — ornith-1.0-35b-q8

Endpoint `http://192.168.0.103:8080` · each cell = pass-rate over the task set + avg tokens/wall per task.

| posture \ OCAP | off | on |
|---|---|---|
| **baseline** | 3/3 pass · 6367 tok · 7s | 3/3 pass · 6187 tok · 4s |
| **tenacity** | 3/3 pass · 6338 tok · 5s | 3/3 pass · 6245 tok · 4s |
| **crew** | 3/3 pass · 6328 tok · 6s | 3/3 pass · 6260 tok · 5s |
| **obsessive** | 3/3 pass · 6249 tok · 4s | 3/3 pass · 6526 tok · 8s |

## Per-cell task detail

| posture | ocap | task | verify | status | tools | writes | tokens | wall |
|---|---|---|---|---|---|---|---|---|
| baseline | off | edit-version | pass | completed | 1 | 1 | 6698 | 11.8 |
| baseline | off | fix-typo | pass | completed | 2 | 1 | 6258 | 4.8 |
| baseline | off | write-greeting | pass | completed | 1 | 1 | 6145 | 3.5 |
| baseline | on | edit-version | pass | completed | 1 | 1 | 6181 | 3.3 |
| baseline | on | fix-typo | pass | completed | 2 | 1 | 6261 | 4.8 |
| baseline | on | write-greeting | pass | completed | 1 | 1 | 6121 | 3.1 |
| tenacity | off | edit-version | pass | completed | 1 | 1 | 6231 | 4.3 |
| tenacity | off | fix-typo | pass | completed | 3 | 1 | 6657 | 7.1 |
| tenacity | off | write-greeting | pass | completed | 1 | 1 | 6126 | 3.2 |
| tenacity | on | edit-version | pass | completed | 1 | 1 | 6204 | 3.7 |
| tenacity | on | fix-typo | pass | completed | 2 | 1 | 6368 | 4.7 |
| tenacity | on | write-greeting | pass | completed | 1 | 1 | 6165 | 3.9 |
| crew | off | edit-version | pass | completed | 1 | 1 | 6208 | 3.8 |
| crew | off | fix-typo | pass | completed | 2 | 1 | 6461 | 7.7 |
| crew | off | write-greeting | pass | completed | 1 | 1 | 6315 | 5.4 |
| crew | on | edit-version | pass | completed | 1 | 1 | 6234 | 4.3 |
| crew | on | fix-typo | pass | completed | 2 | 1 | 6273 | 5.0 |
| crew | on | write-greeting | pass | completed | 1 | 1 | 6273 | 4.6 |
| obsessive | off | edit-version | pass | completed | 1 | 1 | 6208 | 3.8 |
| obsessive | off | fix-typo | pass | completed | 2 | 1 | 6408 | 5.2 |
| obsessive | off | write-greeting | pass | completed | 1 | 1 | 6133 | 3.2 |
| obsessive | on | edit-version | pass | completed | 2 | 1 | 7059 | 16.1 |
| obsessive | on | fix-typo | pass | completed | 2 | 1 | 6391 | 5.1 |
| obsessive | on | write-greeting | pass | completed | 1 | 1 | 6128 | 3.1 |
