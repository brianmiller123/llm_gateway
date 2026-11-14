# Please install OpenAI SDK first: `pip3 install openai`
import os

from openai import OpenAI

# 凭据不入仓：从环境变量读取（曾提交进仓库的 key 应视为泄露并轮换）
api_key = os.environ.get("GATEWAY_API_KEY", "")
if not api_key:
    raise SystemExit("错误: 未设置 GATEWAY_API_KEY 环境变量（export GATEWAY_API_KEY=sk-xxx 后重试）")

client = OpenAI(api_key=api_key, base_url="http://127.0.0.1:8080/v1")

response = client.responses.create(
    model="deepseek-v4-flash",
    instructions="You are a helpful assistant.",
    input="写一段歌手春天的诗",
)

print(response.output_text)
