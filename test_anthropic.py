import anthropic
import os



os.environ['ANTHROPIC_BASE_URL'] = 'http://127.0.0.1:8080'
os.environ['ANTHROPIC_API_KEY'] = 'sk-2e9a7f360d60c28f8cd39871292710bc'


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