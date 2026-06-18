from newt_agent.inference import LocalOllamaBackend, ChatRequest
import asyncio

async def main():
    backend = await LocalOllamaBackend.discover("llama3.1:8b")
    req = ChatRequest()
    req.system("You are a coding assistant.")
    req.user("Hello!")
    reply = await backend.complete(req)
    print(reply.model_id, reply.content)

asyncio.run(main())