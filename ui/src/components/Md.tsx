import ReactMarkdown from "react-markdown";

export function Md({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown>{text}</ReactMarkdown>
    </div>
  );
}
