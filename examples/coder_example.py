from newt_agent.coder import build_prompt, normalize_emission

prompt = build_prompt("rename foo to bar")
print(prompt)

normalized = normalize_emission("Hello, world!")
print(normalized)