import "./App.css";
import { useState, useEffect } from "react";
import { getVersion } from "@tauri-apps/api/app";

function App() {
  const [version, setVersion] = useState<string>("");

  useEffect(() => {
    const fetchVersion = async () => {
      try {
        const appVersion = await getVersion();
        setVersion(appVersion);
      } catch (error) {
        console.error("Failed to get app version:", error);
        setVersion("0.0.0");
      }
    };

    fetchVersion();
  }, []);

  return (
    <main className="landing">
      <div className="hero">
        <h1 className="title">dalzzack</h1>
        <p className="subtitle">Browser Automation CLI for AI Agents</p>
      </div>

      <div className="description">
        <p>
          <strong>Tauri WebView</strong> CLI &mdash; AI agents control a real browser
          for navigation, form input, screenshot, network capture,
          iframe access, and captcha processing.
        </p>
      </div>

      <div className="section-label">Browser Control</div>
      <div className="tools-grid cols-4">
        <div className="tool-card">
          <div className="tool-icon">&#x1F310;</div>
          <h3>navigate</h3>
          <p>URL navigation</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F4C4;</div>
          <h3>get_html</h3>
          <p>HTML capture</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x26A1;</div>
          <h3>execute_js</h3>
          <p>JS execution</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F4F7;</div>
          <h3>screenshot</h3>
          <p>xcap capture</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F517;</div>
          <h3>get_current_url</h3>
          <p>URL &amp; title</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F4E1;</div>
          <h3>get_captured_requests</h3>
          <p>XHR/fetch log</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F5BC;</div>
          <h3>get_frame_list</h3>
          <p>iframe list</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F4E6;</div>
          <h3>get_frame_html</h3>
          <p>iframe HTML</p>
        </div>
      </div>

      <div className="section-label">Interaction &amp; Processing</div>
      <div className="tools-grid cols-4">
        <div className="tool-card">
          <div className="tool-icon">&#x23F3;</div>
          <h3>wait</h3>
          <p>Timed delay</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F50D;</div>
          <h3>wait_for_selector</h3>
          <p>DOM polling</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F4DD;</div>
          <h3>fill_form</h3>
          <p>SPA input</p>
        </div>
        <div className="tool-card">
          <div className="tool-icon">&#x1F9E9;</div>
          <h3>preprocess_captcha</h3>
          <p>Image pipeline</p>
        </div>
      </div>

      <div className="footer">
        <p className="status">dalzzack Ready</p>
        <p className="version">v{version} &middot; 24 Tools</p>
      </div>
    </main>
  );
}

export default App;
