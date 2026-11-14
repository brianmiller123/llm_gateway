import anthropic
import os

# 凭据不入仓：从环境变量读取（曾提交进仓库的 key 应视为泄露并轮换）
API_KEY = os.environ.get("GATEWAY_API_KEY", "")
if not API_KEY:
    raise SystemExit("错误: 未设置 GATEWAY_API_KEY 环境变量（控制台 → API Keys 创建后 export GATEWAY_API_KEY=sk-xxx）")


os.environ['ANTHROPIC_BASE_URL'] = 'http://127.0.0.1:8080'
os.environ['ANTHROPIC_API_KEY'] = API_KEY


client = anthropic.Anthropic()

message = client.messages.create(
    model="deepseek-v4-pro",
    max_tokens=1000,
    system="You are a helpful assistant.",
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Hi, how are you?"
                }
            ]
        }
    ]
)
print(message.content)