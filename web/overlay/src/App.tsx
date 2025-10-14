import './App.css';

const healthEndpoint = `${window.location.origin.replace(/:\d+$/, ':8080')}/healthz`;

function App() {
  return (
    <div className="overlay">
      <h1>Twitch Overlay Scaffold</h1>
      <p>
        Backend health check:{' '}
        <a href={healthEndpoint}>{healthEndpoint}</a>
      </p>
    </div>
  );
}

export default App;
