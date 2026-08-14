# Testing You.com Integration

## Basic search without API key (should use keyless endpoint)
./target/debug/orx lit "machine learning papers" --source youcom --limit 3

## Expected behavior:
- Uses https://api.you.com/v1/agents/search endpoint (keyless)
- Returns 3 web search results formatted as literature hits
- Each result should have title, URL (as id), and snippet (as abstract)

## With API key (if YDC_API_KEY is set):
export YDC_API_KEY="your-key-here"
./target/debug/orx lit "transformers attention mechanism" --source youcom --limit 3

## Expected behavior with API key:
- Uses https://api.you.com/v1/search endpoint (authenticated)
- Should provide more comprehensive results

