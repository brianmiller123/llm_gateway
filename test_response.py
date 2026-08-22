# Please install OpenAI SDK first: `pip3 install openai`
from openai import OpenAI

client = OpenAI(api_key="sk-2e9a7f360d60c28f8cd39871292710bc", base_url="http://127.0.0.1:8080/v1")

response = client.responses.create(
    model="deepseek-v4-flash",
    instructions="You are a helpful assistant.",
    input="写一段歌手春天的诗",
)

print(response.output_text)
